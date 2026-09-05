// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Measures the successor-locks caught-up check.
//!
//! The check places a branch in one of three relations to a file's chain —
//! caught up, ahead, or behind. Caught up is a hash comparison against the
//! file's content at the branch tip. The other two walk the file's
//! content-changing history backwards, under a bound: a revision numbered below
//! the chain's revision cannot be the chain's revision or descend from it, so
//! the walk has passed the point where the chain's content could appear.
//!
//! The design claims the caught-up path reads no history, that a hop is a fixed
//! number of reads whatever the repository holds, and that the bound is what
//! keeps a denial from walking a whole branch. All three are claims about a real
//! tree with real revisions behind it.
//!
//! Time here is a floor, not a forecast: the stores are in-memory, so every read
//! is a memcpy where a deployment has a block store and a cache behind it. The
//! portable numbers are the **read counts** — states deserialized, file-metadata
//! blocks and node blocks touched — since those are what a deployment turns into
//! store round trips.
//!
//! ```text
//! cargo test --release -p lore-revision --test caught_up_bench -- --ignored --nocapture
//! ```
//!
//! `CAUGHT_UP_FILES` sets the repository size, `CAUGHT_UP_DEPTH` the number of
//! revisions that modify the measured file.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::BranchId;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::commit::commit_in_memory_revision;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::*;
    use lore_revision::repository::InMemoryContext;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

    struct InMemoryMarker;
    impl InMemoryContext for InMemoryMarker {}
    const IN_MEMORY_MARKER: InMemoryMarker = InMemoryMarker;

    async fn test_repository(
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        Arc::new(
            RepositoryContext::new(default_repository_creation_args(
                immutable_store,
                mutable_store,
            ))
            .with_write_token(RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)),
        )
    }

    fn token() -> RepositoryWriteToken {
        RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)
    }

    fn metadata_on(branch: BranchId) -> Metadata {
        let mut metadata = Metadata::new();
        metadata
            .set_branch(branch)
            .expect("setting the branch must succeed");
        metadata
    }

    fn file(name: &str, content: Hash) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 10,
            address: Address {
                hash: content,
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    fn directory(name: &str) -> Node {
        Node {
            flags: NodeFlags::NoFlags.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    /// `node_add` alone leaves neither the staged action nor the dirty ancestors
    /// the freeze walks, so every fixture addition records both.
    async fn add(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> NodeID {
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        state
            .node_mark_staged(
                repository,
                node_id,
                NodeFlags::StagedAdd,
                NodeFlags::DirtyAdd,
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    /// The lookups a walk asks for. A lookup is not always a store read: a
    /// `State` caches its blocks by weak reference, so a lookup for a block
    /// something else is still holding is a hit. Counting requests and timing
    /// them separately is what shows the difference.
    #[derive(Default)]
    struct Reads {
        states: AtomicUsize,
        metadata_blocks: AtomicUsize,
        nodes: AtomicUsize,
    }

    impl Reads {
        fn take(&self) -> (usize, usize, usize) {
            (
                self.states.swap(0, Ordering::Relaxed),
                self.metadata_blocks.swap(0, Ordering::Relaxed),
                self.nodes.swap(0, Ordering::Relaxed),
            )
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Relation {
        CaughtUp,
        Ahead,
        Behind,
        Indeterminate,
    }

    /// The check as the spec defines it.
    ///
    /// The tip's content answers caught-up without opening a revision. Failing
    /// that, the walk follows the file-history back-pointer, which skips every
    /// revision that did not touch the file, and stops at the first revision
    /// numbered below the chain's — which can neither be the chain's revision
    /// nor descend from it.
    async fn caught_up(
        repository: &Arc<RepositoryContext>,
        tip: Arc<State>,
        node_id: NodeID,
        chain_content: Hash,
        chain_revision_number: u64,
        budget: usize,
        reads: &Reads,
    ) -> (Relation, usize) {
        reads.nodes.fetch_add(1, Ordering::Relaxed);
        let at_tip = tip
            .node(repository.clone(), node_id)
            .await
            .expect("the file must read back at the tip");
        if at_tip.address.hash == chain_content {
            return (Relation::CaughtUp, 0);
        }

        let mut state = tip;
        let mut hops = 0usize;
        loop {
            if hops >= budget {
                return (Relation::Indeterminate, hops);
            }

            reads.metadata_blocks.fetch_add(1, Ordering::Relaxed);
            let previous = state
                .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(node_id))
                .await
                .expect("the file metadata block must read back")
                .node(NodeFileMetadata::index(node_id))
                .revision[0];
            if previous.is_zero() {
                // The file's history ends without reaching the chain's content.
                return (Relation::Behind, hops);
            }

            reads.states.fetch_add(1, Ordering::Relaxed);
            let previous_state = State::deserialize(repository.clone(), previous)
                .await
                .expect("a revision in the file's history must deserialize");
            hops += 1;

            reads.nodes.fetch_add(1, Ordering::Relaxed);
            let at_hop = previous_state
                .node(repository.clone(), node_id)
                .await
                .expect("the file must read back at the hop");
            if at_hop.address.hash == chain_content {
                return (Relation::Ahead, hops);
            }
            if previous_state.revision_number() < chain_revision_number {
                return (Relation::Behind, hops);
            }

            state = previous_state;
        }
    }

    /// One revision's identity, as the chain would record it.
    struct Step {
        revision_number: u64,
        content: Hash,
    }

    fn per(total: Duration, n: usize) -> Duration {
        total / (n.max(1) as u32)
    }

    /// Averages over enough runs that a single scheduling artifact does not
    /// carry the number, and returns the reads one run costs.
    async fn measure(
        repository: &Arc<RepositoryContext>,
        tip: &Arc<State>,
        node_id: NodeID,
        content: Hash,
        revision_number: u64,
        runs: usize,
        expected: Relation,
        expected_hops: Option<usize>,
    ) -> (Duration, usize, (usize, usize, usize)) {
        let reads = Reads::default();
        // One warm run, both to fill any cache the walk shares with a live
        // server and to check the relation before it is timed.
        let (relation, hops) = caught_up(
            repository,
            tip.clone(),
            node_id,
            content,
            revision_number,
            usize::MAX,
            &reads,
        )
        .await;
        assert_eq!(relation, expected, "the walk must reach the right relation");
        if let Some(want) = expected_hops {
            assert_eq!(hops, want, "the walk must take the expected hops");
        }
        let cost = reads.take();

        let start = Instant::now();
        for _ in 0..runs {
            caught_up(
                repository,
                tip.clone(),
                node_id,
                content,
                revision_number,
                usize::MAX,
                &reads,
            )
            .await;
        }
        let elapsed = start.elapsed();
        reads.take();
        (per(elapsed, runs), hops, cost)
    }

    /// Builds a repository of `files` files whose measured file is then modified
    /// by `depth` successive revisions, and reports what each relation costs.
    async fn run_shape(
        mutable: Arc<dyn lore_storage::MutableStore>,
        files: usize,
        depth: usize,
        runs: usize,
    ) {
        let repository = test_repository(mutable).await;
        let branch = Context::from(uuid::Uuid::now_v7());
        let state = Arc::new(State::new());

        // A tree of `files` files spread over directories, so the measured file
        // sits in a repository rather than alone.
        let mut measured = NodeID::default();
        let directories = 64usize;
        let mut dirs = Vec::with_capacity(directories);
        for d in 0..directories {
            let name = format!("dir_{d}");
            dirs.push(
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory(&name),
                    &name,
                )
                .await,
            );
        }
        for i in 0..files {
            let name = format!("asset_{i}.uasset");
            let node = add(
                &state,
                repository.clone(),
                dirs[i % directories],
                file(&name, Hash::from_u64(i as u64 + 1)),
                &name,
            )
            .await;
            // The measured file sits in the middle, so its node block is no more
            // resident than any other.
            if i == files / 2 {
                measured = node;
            }
        }

        let build = Instant::now();
        let mut revision = commit_in_memory_revision(
            repository.clone(),
            &token(),
            state.clone(),
            metadata_on(branch),
            Hash::default(),
            branch,
        )
        .await
        .expect("the base revision must commit");
        let base = build.elapsed();

        let mut steps = Vec::with_capacity(depth + 1);
        {
            let published = State::deserialize(repository.clone(), revision)
                .await
                .expect("the base revision must deserialize");
            steps.push(Step {
                revision_number: published.revision_number(),
                content: Hash::from_u64((files / 2) as u64 + 1),
            });
        }

        // Every step modifies the measured file, so each is a hop the walk has to
        // take — the worst case for a file of its age.
        let edits_started = Instant::now();
        for step in 0..depth {
            let content = Hash::from_u64(0xC0FFEE + step as u64);
            let node = state
                .node(repository.clone(), measured)
                .await
                .expect("the measured file must read back");
            state
                .node_modify(
                    repository.clone(),
                    measured,
                    node.mode,
                    node.size + 1,
                    Address {
                        hash: content,
                        context: node.address.context,
                    },
                )
                .await
                .expect("modifying the measured file must succeed");
            let (staged, dirty) = State::staged_edit_flags(&node);
            state
                .node_mark_staged(repository.clone(), measured, staged, dirty)
                .await
                .expect("marking the modification must succeed");

            revision = commit_in_memory_revision(
                repository.clone(),
                &token(),
                state.clone(),
                metadata_on(branch),
                revision,
                branch,
            )
            .await
            .expect("the edit must commit");

            steps.push(Step {
                revision_number: state.revision_number(),
                content,
            });
        }
        let edits = edits_started.elapsed();

        let tip = State::deserialize(repository.clone(), revision)
            .await
            .expect("the tip must deserialize");

        println!("\n{files} files, {depth} revisions modifying one file");
        println!(
            "  fixture     base commit {base:.2?}, {depth} edits {edits:.2?} ({:.2?} each)",
            per(edits, depth)
        );

        // What a hop's dominant read costs on its own, so the walk's slope can be
        // attributed rather than guessed.
        let parent = tip.parent_self();
        let start = Instant::now();
        for _ in 0..runs {
            State::deserialize(repository.clone(), parent)
                .await
                .expect("the parent revision must deserialize");
        }
        let deserialize = per(start.elapsed(), runs);

        // A hop is a deserialize plus two block reads against a state that has
        // just been opened, so both blocks come from the store rather than from
        // the state's own cache. Timed apart, because the split decides whether
        // a batch can amortize the hop or has to pay it per file.
        let start = Instant::now();
        for _ in 0..runs {
            let fresh = State::deserialize(repository.clone(), parent)
                .await
                .expect("the parent revision must deserialize");
            fresh
                .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(measured))
                .await
                .expect("the metadata block must read back");
        }
        let cold_metadata = per(start.elapsed(), runs).saturating_sub(deserialize);

        let start = Instant::now();
        for _ in 0..runs {
            let fresh = State::deserialize(repository.clone(), parent)
                .await
                .expect("the parent revision must deserialize");
            fresh
                .node(repository.clone(), measured)
                .await
                .expect("the node must read back");
        }
        let cold_node = per(start.elapsed(), runs).saturating_sub(deserialize);

        // The same two reads against a state already opened, which is what a
        // second file on the same hop would cost.
        let warm = State::deserialize(repository.clone(), parent)
            .await
            .expect("the parent revision must deserialize");
        warm.block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(measured))
            .await
            .expect("the metadata block must read back");
        warm.node(repository.clone(), measured)
            .await
            .expect("the node must read back");
        let start = Instant::now();
        for _ in 0..runs {
            warm.block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(measured))
                .await
                .expect("the metadata block must read back");
            warm.node(repository.clone(), measured)
                .await
                .expect("the node must read back");
        }
        let warm_reads = per(start.elapsed(), runs);

        println!(
            "  one hop     deserialize {deserialize:.2?} + metadata block {cold_metadata:.2?} + node block {cold_node:.2?}"
        );
        println!("              both blocks again on the open state {warm_reads:.2?}\n");

        println!("  relation           hops        time   states  meta  nodes");

        // Caught up: the chain's content is what the tip holds.
        let (time, hops, (states, blocks, nodes)) = measure(
            &repository,
            &tip,
            measured,
            steps[depth].content,
            steps[depth].revision_number,
            runs,
            Relation::CaughtUp,
            Some(0),
        )
        .await;
        println!("  caught up       {hops:>7}   {time:>9.2?}   {states:>6} {blocks:>5} {nodes:>6}");

        // Ahead: the chain sits d content-changing revisions back and the branch
        // has moved past it.
        let mut d = 1usize;
        while d <= depth {
            let step = &steps[depth - d];
            let (time, hops, (states, blocks, nodes)) = measure(
                &repository,
                &tip,
                measured,
                step.content,
                step.revision_number,
                runs,
                Relation::Ahead,
                Some(d),
            )
            .await;
            println!(
                "  ahead at {d:<5}  {hops:>7}   {time:>9.2?}   {states:>6} {blocks:>5} {nodes:>6}"
            );
            d *= 2;
        }

        // Behind, bounded. The bound is strict, so the walk stops one hop past
        // the chain's own revision: it has to reach a revision numbered below it
        // to have proven anything.
        let mut d = 1usize;
        while d <= depth {
            let step = &steps[depth - d];
            let (time, hops, (states, blocks, nodes)) = measure(
                &repository,
                &tip,
                measured,
                Hash::from_u64(0xDEAD_BEEF),
                step.revision_number,
                runs,
                Relation::Behind,
                Some((d + 1).min(depth)),
            )
            .await;
            println!(
                "  behind at {d:<4}  {hops:>7}   {time:>9.2?}   {states:>6} {blocks:>5} {nodes:>6}"
            );
            d *= 2;
        }

        // Behind with no bound to save it: a chain entry older than anything on
        // the branch walks the file's whole history.
        let (time, hops, (states, blocks, nodes)) = measure(
            &repository,
            &tip,
            measured,
            Hash::from_u64(0xDEAD_BEEF),
            0,
            runs,
            Relation::Behind,
            Some(depth),
        )
        .await;
        println!(
            "  behind, unbounded {hops:>5}   {time:>9.2?}   {states:>6} {blocks:>5} {nodes:>6}"
        );
    }

    /// What a batch holds on to between files.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Hold {
        /// Each file walks alone, as a per-file check would.
        Nothing,
        /// Each revision is deserialized once for the whole batch.
        States,
        /// Revisions and the blocks read out of them are both kept alive.
        StatesAndBlocks,
        /// As above, but the batch is walked in node-block order and the held
        /// blocks are dropped when the walk leaves a block. A batch cannot
        /// assume its files are near each other in the tree, but it can put them
        /// in that order itself.
        SortedBlocks,
    }

    impl Hold {
        fn label(self) -> &'static str {
            match self {
                Hold::Nothing => "nothing held",
                Hold::States => "revisions held",
                Hold::StatesAndBlocks => "revisions + blocks",
                Hold::SortedBlocks => "block-sorted",
            }
        }
    }

    /// The walk for a whole acquisition rather than a single file.
    ///
    /// A `State` holds its blocks by weak reference, so keeping the revision
    /// alive saves the deserialize but not the block reads: the next file in the
    /// same block re-reads it unless something is still holding the block. The
    /// three strategies separate those two savings.
    async fn batch_walk(
        repository: &Arc<RepositoryContext>,
        tip: &Arc<State>,
        nodes: &[NodeID],
        chain_content: Hash,
        chain_revision_number: u64,
        hold: Hold,
        reads: &Reads,
    ) -> (usize, usize) {
        let cache = hold != Hold::Nothing;
        let keep_blocks = matches!(hold, Hold::StatesAndBlocks | Hold::SortedBlocks);
        // Walking in node-block order means the blocks for one index are wanted
        // together, so the resident set is one index deep rather than the batch.
        let mut ordered = nodes.to_vec();
        if hold == Hold::SortedBlocks {
            ordered.sort_unstable_by_key(|id| NodeBlock::index(*id));
        }
        let nodes = &ordered[..];
        let mut current_block = usize::MAX;
        let mut peak_resident = 0usize;
        let mut states: std::collections::HashMap<Hash, Arc<State>> =
            std::collections::HashMap::new();
        // Held only to keep the state's weak block references alive. Keyed by
        // revision and block index so the count is distinct blocks resident,
        // which is what the memory bound is written against.
        let mut held_meta: std::collections::HashMap<(Hash, usize), Arc<NodeFileMetadataBlock>> =
            std::collections::HashMap::new();
        let mut held_nodes: std::collections::HashMap<(Hash, usize), Arc<NodeBlock>> =
            std::collections::HashMap::new();
        let mut total = 0usize;
        for node_id in nodes {
            if hold == Hold::SortedBlocks {
                let block_index = NodeBlock::index(*node_id);
                if block_index != current_block {
                    peak_resident = peak_resident.max(held_meta.len() + held_nodes.len());
                    held_meta.clear();
                    held_nodes.clear();
                    current_block = block_index;
                }
            }
            let mut state = tip.clone();
            reads.nodes.fetch_add(1, Ordering::Relaxed);
            let at_tip = state
                .node(repository.clone(), *node_id)
                .await
                .expect("the file must read back at the tip");
            if at_tip.address.hash == chain_content {
                continue;
            }
            loop {
                reads.metadata_blocks.fetch_add(1, Ordering::Relaxed);
                let metadata_block = state
                    .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(*node_id))
                    .await
                    .expect("the file metadata block must read back");
                let previous = metadata_block
                    .node(NodeFileMetadata::index(*node_id))
                    .revision[0];
                if keep_blocks {
                    held_meta.insert(
                        (state.revision(), NodeFileMetadataBlock::index(*node_id)),
                        metadata_block,
                    );
                }
                if previous.is_zero() {
                    break;
                }
                let previous_state = match states.get(&previous) {
                    Some(hit) if cache => hit.clone(),
                    _ => {
                        reads.states.fetch_add(1, Ordering::Relaxed);
                        let opened = State::deserialize(repository.clone(), previous)
                            .await
                            .expect("a revision in the file's history must deserialize");
                        if cache {
                            states.insert(previous, opened.clone());
                        }
                        opened
                    }
                };
                total += 1;
                reads.nodes.fetch_add(1, Ordering::Relaxed);
                let at_hop = if keep_blocks {
                    let block = previous_state
                        .block(repository.clone(), NodeBlock::index(*node_id))
                        .await
                        .expect("the node block must read back");
                    let node = *block.read().node(Node::index(*node_id));
                    held_nodes.insert((previous, NodeBlock::index(*node_id)), block);
                    node
                } else {
                    previous_state
                        .node(repository.clone(), *node_id)
                        .await
                        .expect("the file must read back at the hop")
                };
                if at_hop.address.hash == chain_content
                    || previous_state.revision_number() < chain_revision_number
                {
                    break;
                }
                state = previous_state;
            }
        }
        (total, peak_resident.max(held_meta.len() + held_nodes.len()))
    }

    /// Measures a batch acquisition's worst case: every file denied, so every
    /// file walks its whole history.
    ///
    /// `scattered` decides whether the batch's files share node blocks. Files
    /// added together get adjacent ids and share them; a prefab that locks
    /// assets from all over the tree does not, and the design assumes no
    /// locality, so both are measured.
    async fn run_batch_shape(
        mutable: Arc<dyn lore_storage::MutableStore>,
        files: usize,
        batch: usize,
        depth: usize,
        scattered: bool,
    ) {
        let repository = test_repository(mutable).await;
        let branch = Context::from(uuid::Uuid::now_v7());
        let state = Arc::new(State::new());

        let directories = 64usize;
        let mut dirs = Vec::with_capacity(directories);
        for d in 0..directories {
            let name = format!("dir_{d}");
            dirs.push(
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory(&name),
                    &name,
                )
                .await,
            );
        }
        let mut all = Vec::with_capacity(files);
        for i in 0..files {
            let name = format!("asset_{i}.uasset");
            all.push(
                add(
                    &state,
                    repository.clone(),
                    dirs[i % directories],
                    file(&name, Hash::from_u64(i as u64 + 1)),
                    &name,
                )
                .await,
            );
        }

        // Contiguous ids share node blocks; one per block shares none.
        // Contiguous ids land a batch in as few node blocks as the tree allows.
        // Scattering spreads it across the tree instead, which is what locking a
        // prefab's assets does.
        let stride = if scattered {
            (files / batch.max(1)).max(1)
        } else {
            1
        };
        let chosen: Vec<NodeID> = all.iter().copied().step_by(stride).take(batch).collect();
        let batch = chosen.len();
        assert!(batch > 0, "the tree must supply a batch");
        let blocks: std::collections::HashSet<usize> =
            chosen.iter().map(|id| NodeBlock::index(*id)).collect();

        let mut revision = commit_in_memory_revision(
            repository.clone(),
            &token(),
            state.clone(),
            metadata_on(branch),
            Hash::default(),
            branch,
        )
        .await
        .expect("the base revision must commit");

        for step in 0..depth {
            for (index, node_id) in chosen.iter().enumerate() {
                let node = state
                    .node(repository.clone(), *node_id)
                    .await
                    .expect("the file must read back");
                state
                    .node_modify(
                        repository.clone(),
                        *node_id,
                        node.mode,
                        node.size + 1,
                        Address {
                            hash: Hash::from_u64(0xC0FFEE + (step * batch + index) as u64),
                            context: node.address.context,
                        },
                    )
                    .await
                    .expect("modifying the file must succeed");
                let (staged, dirty) = State::staged_edit_flags(&node);
                state
                    .node_mark_staged(repository.clone(), *node_id, staged, dirty)
                    .await
                    .expect("marking the modification must succeed");
            }
            revision = commit_in_memory_revision(
                repository.clone(),
                &token(),
                state.clone(),
                metadata_on(branch),
                revision,
                branch,
            )
            .await
            .expect("the edit must commit");
        }

        let tip = State::deserialize(repository.clone(), revision)
            .await
            .expect("the tip must deserialize");

        let shape = if scattered { "scattered" } else { "contiguous" };
        println!(
            "\n  {batch} files {shape} over {} node blocks, {depth} revisions each, {files}-file tree",
            blocks.len()
        );

        for hold in [
            Hold::Nothing,
            Hold::States,
            Hold::StatesAndBlocks,
            Hold::SortedBlocks,
        ] {
            let reads = Reads::default();
            let start = Instant::now();
            let (hops, resident) = batch_walk(
                &repository,
                &tip,
                &chosen,
                Hash::from_u64(0xDEAD_BEEF),
                0,
                hold,
                &reads,
            )
            .await;
            let elapsed = start.elapsed();
            let (states, meta, nodes) = reads.take();
            println!(
                "    {:<19} {elapsed:>9.2?} total, {:>9.2?} per file   {hops} hops, {states} deserializes, {} lookups, {resident} blocks resident ({:.0} MB)",
                hold.label(),
                per(elapsed, batch),
                meta + nodes,
                (resident * 64 * 1024) as f64 / (1024.0 * 1024.0),
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "measurement, not a test — run explicitly"]
    async fn caught_up_check_scale() {
        let shapes: Vec<usize> = std::env::var("CAUGHT_UP_FILES")
            .ok()
            .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![10_000, 100_000]);
        let depth: usize = std::env::var("CAUGHT_UP_DEPTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64);
        let runs: usize = std::env::var("CAUGHT_UP_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);
        let batch: usize = std::env::var("CAUGHT_UP_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2_000);
        let batch_depth: usize = std::env::var("CAUGHT_UP_BATCH_DEPTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let batch_tree: usize = std::env::var("CAUGHT_UP_BATCH_TREE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);

        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                println!("\nsuccessor-locks caught-up check");
                println!(
                    "in-memory stores: times are a floor, read counts are what a deployment pays"
                );
                for files in shapes {
                    run_shape(mutable.clone(), files, depth, runs).await;
                }

                println!("\nbatch acquisition, every file denied so every file walks");
                for scattered in [false, true] {
                    run_batch_shape(mutable.clone(), batch_tree, batch, batch_depth, scattered)
                        .await;
                }
                println!();
            }))
            .await
            .expect("Task failed");
    }
}
