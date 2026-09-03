---
lep: 2026-06-19-successor-locks-unmergeable-files
title: Successor Locks for Unmergeable Files
authors:
  - mattias.jansson
status: Accepted
created: 2026-06-19
updated: 2026-09-03
discussion: https://github.com/EpicGames/lore/pull/39
---

# Successor Locks for Unmergeable Files

## Summary

This proposal adds causal exclusive locks for unmergeable files in Lore's free branching model. Each unmergeable file gains a server-side per-file "latest" pointer naming the latest revision that modified it within a *lock scope*. An exclusive lock can only be acquired on a branch whose last-modification revision for the file (looked up via Lore's existing per-file-id back-pointer index) equals that latest — i.e. the branch already contains every prior committed edit in scope. Each unmergeable file becomes one linear edit chain per scope, embedded in the larger DAG; the lock acquisition is the gate that enforces successor-only progress within the scope. Scopes are first-class server-side entities with their own identity and lifecycle, independent of any specific branch; each branch carries a `scope_id` in its metadata, inherited from its parent at creation. A repository has a default scope that all branches join unless they explicitly opt into another scope; a repository that only ever uses the default scope behaves identically to a global per-file chain (the degenerate case). The check is constant-bounded — a single revision-id comparison against Lore's existing back-pointer lookup — with no path-keyed state and no materialized (branch × file) indices.

## Motivation

Lore supports free branching: any user creates a branch, edits, and requests a merge. For files that merge by content (text, structured data), divergent edits across branches resolve at merge time. For unmergeable files — binary assets, opaque tool-managed formats — there is no such resolution: two divergent edits cannot be combined, so the only safe outcome is that one branch's edit becomes the new state and the other's work is discarded.

Lore's current exclusive-lock primitive prevents (through opt-in compliance) two concurrent edits to the same file by serializing access while the lock is held. It does not prevent the same file from being edited in parallel across branches over time. Locks are ephemeral: released on commit or push, after which any branch can acquire a fresh lock on the same file. Whether the lock is scoped globally or per-branch makes no difference — the contention is causal, not temporal. A user on branch B can hold an exclusive lock on file F, commit an edit, release the lock, and after this point in time a user on branch C — which has never observed B's edit — can acquire a new lock on F, edit it independently, and commit. The two edits now sit as competing tips of an unmergeable file's history. When B and C eventually merge, the conflict has no resolution path that does not silently lose work, even though the locks were correctly acquired and released.

The gap is not in exclusion. The current primitive correctly answers "is it safe to edit this file right now?" The gap is in causality: nothing answers "is it safe to take a new lock on this file from this branch — have all prior edits been made visible here?" Without that answer, exclusive locks alone cannot keep an unmergeable file's history coherent under free branching.

A second gap follows from the first. Even with a causality primitive in place, a single global chain per file over-couples lines of work that should not block each other. Bug fixes to an unmergeable file on a long-lived release branch would force every editor on main to be downstream of the release-branch fix before they could lock the file — and vice versa. The same coupling makes free-form experimentation impossible: a designer trying out alternatives on a throwaway branch would freeze edits to the same assets across the rest of the repository until the experiment was merged or abandoned. The shape of the second gap is partitioning: the chain mechanism needs a way to scope its causality so that independent lines of work do not interfere with each other's editability.

This matters now because Lore targets workflows with large unmergeable working sets — game assets, design files, generated artifacts — where the cost of an unresolvable conflict at merge time is far higher than the cost of upfront serialization, and where parallel release lines and isolated experiments are standard practice. Without both a causality primitive and a scoping primitive, lock-based protection of unmergeable files in Lore is structurally incomplete at any scale.

## Goals / Non-Goals

### Goals

1. **Causal-safety check at lock acquisition.** Before granting an exclusive lock on file F on branch B, verify that B contains every prior committed edit to F.
2. **Single linear edit chain per unmergeable file.** Committed edits to an unmergeable file form one totally ordered chain across the repository, regardless of how the surrounding DAG branches.
3. **Constant-bounded lock check at scale.** Tens of millions of files, tens of thousands of branches, high churn — lock check is one KV read plus one Merkle traversal, with no (branch × file) materialization and no global broadcast on push.
4. **Content-only chain advance.** Modifying the *content* of an unmergeable file (its BLAKE3 hash on the tree leaf node) advances the chain and requires the lock. Metadata-only changes — mode, timestamps, extended attributes — and path-of-record changes (rename, move) are *not* chain-advancing; they remain outside the lock protocol and merge through Lore's normal tree-merge mechanisms.
5. **Identity by file_id, never by path.** All cross-branch state keys on the stable file_id (which survives moves) — path is input/output only.
6. **Actionable lock-denial errors.** A denied lock names the revision, branch, and scope of the current latest, so the user knows what to merge or sync.
7. **Scope-partitioned chains.** Independent lines of work — release branches, experimentation sandboxes — maintain independent chains for the same file. Edits in one scope never block lock acquisition in another. Scopes are separate entities with their own lifecycle; each branch carries a `scope_id` as metadata, defaulting to the parent's scope at creation. A repository that only uses the default scope behaves identically to the global per-file chain model.
8. **Server-mediated enforcement.** Lock validation and lock release are integrated into the Lore server's push handler — not advisory client-side checks. The server is the only path that mutates latest and lock state, so clients cannot bypass the protocol by skipping the lock-acquire flow. This elevates the existing exclusive-lock primitive (which Motivation describes as relying on "opt-in compliance") to a first-class protocol-state mechanism.
9. **Policy-controlled strictness.** The strictness with which each primitive (lock state, chain latest) gates operations is a repository-policy choice (see Enforcement policy in Proposed Design). Under the default strict policy, the server rejects pushes lacking the held lock or whose parent does not match the current latest. Under advisory policy on either primitive, the server emits a warning and audit-logs in the same situations but allows the operation. The server remains the authority either way — what changes is the response, not whether the server is in the loop.

### Non-Goals

- Define a general file-attribute system. Unmergeability itself can't be left open — the server evaluates it on every push — so this proposal settles it narrowly under [Proposed Design](#unmergeability), and anything broader stays out of scope.
- Auto-merge prior edits on the requester's behalf when the lock check fails. The proposal requires the merge to have happened; tooling can suggest it.
- Change locking behaviour for mergeable files. They remain unaffected and remain outside the locking mechanism.
- Eliminate the operational concerns around lock leases, heartbeats, and zombie cleanup. Those are orthogonal lock-state mechanics.
- Define the wire-protocol details of new RPCs. Scoped to the downstream spec.
- Define transparent (auto-acquired) locks and coupling to file modification notifications. Integrating lock acquisition with file-edit notifications — so a lock is taken automatically when a user opens an unmergeable file for editing, and released on the next push — is a follow-on design. The model proposed here supports such an integration directly (locks are server-side primitives that can be triggered by any client signal; push already releases the lock as a first-class step), but the notification protocol, IDE / tool integration, and UX details live in a future LEP.

## Proposed Design

Lore tracks two separate primitives, not just locks.

┌────────────┬───────────────────────────────────────────────────────────────────────────┬──────────────────────────┐
│ Primitive  │                             What it surfaces                              │      Temporal role       │
├────────────┼───────────────────────────────────────────────────────────────────────────┼──────────────────────────┤
│ Lock       │ "F is being modified right now by branch B"                               │ Present-tense, in-flight │
├────────────┼───────────────────────────────────────────────────────────────────────────┼──────────────────────────┤
│ Causality  │ "F has been modified up through revision X; your branch's view is at < X" │ Past-tense, settled      │
└────────────┴───────────────────────────────────────────────────────────────────────────┴──────────────────────────┘

- Locks address synchronization — coordination of ongoing work. A lock surface answers "is anyone editing F right now?" — which is useful for coordination, UI displays, "ping the lock holder" flows, etc.
- Causality addresses versioning — coordination of historical state. The change-tracking chain answers "is my version current?" — which is useful for status displays, pre-flight checks, "do I need to sync/merge before I start?"

These primitives are realized by two coupled mechanisms: a **scope system** that partitions causality across independent lines of work, and a **change-tracking chain** that records the per-(scope, file) sequence of committed edits. Each is detailed below; the lock-acquisition check joins them.

The **scope system** partitions locking causality into independent regions. A scope is a first-class server-side entity with a stable id and its own lifecycle, independent of any branch. Every branch carries a `scope_id` in its metadata; at branch creation, the new branch inherits its parent's `scope_id` by default, or names a different scope explicitly. Two branches in the same scope share lock causality for unmergeable files; two branches in different scopes do not. A default scope is created automatically at repository init; the repository's initial branch is assigned to it, so every subsequently created branch also lands in the default scope by inheritance unless it opts elsewhere. A repository that never creates other scopes operates as a single global causality region, the degenerate case.

The **change-tracking chain** is the causality primitive, the conceptual linear sequence of committed edits to an unmergeable file within a scope. Each link is a revision that modified the file; the chain is linear by construction because the lock protocol prevents divergent links (a new link can only be added from a branch that already contains the current tip). The chain itself is *not* materialized as a separate data structure — its links are just regular revisions in the revision graph, indistinguishable from any other commit. What the server materializes is one record per live `(scope_id, file_id)` pair: the **latest**, a pointer to the chain's current tip (a revision hash). A branch is "caught up" on F in its scope when the most recent F-modifying revision in its history equals the latest — answered directly by Lore's existing per-file-id back-pointer index, which walks file-history blocks to return, for any revision, the latest revision in its history that modified a given file_id (`lore-revision/src/revision.rs::find_last_modified_revision`, supported by the file-history machinery in `lore-revision/src/file/history.rs`). The check is a single revision-id equality. The chain advances by acquiring an exclusive lock, committing the edit, and pushing: the push atomically writes the new latest value (in Lore's mutable store) and releases the lock. The chain is the protocol-level abstraction; the latest entry plus Lore's back-pointer index are everything required to enforce it.

The lock-acquisition check joins the two mechanisms: a lock is granted only if the requester is caught up on the (scope, file) chain for its own scope. Cross-scope interaction happens only at merge time, where a merge bringing unmergeable-file edits across scope boundaries becomes a fresh chain advance in the target scope (see Cross-scope merges below).

*Figure 1. Three scopes, each with its own independent `latest(F)` for the same unmergeable file F. Branches inherit scope from their parent at creation; lock acquisition compares the branch's last-modification revision for F against `latest(F)` in the branch's own scope only.*

```
┌─────────────────────────────┐  ┌─────────────────────────────┐  ┌─────────────────────────────┐
│  scope: main                │  │  scope: release/1.0         │  │  scope: experiment          │
│  ──────────────             │  │  ──────────────────         │  │  ──────────────             │
│   main ──► feat-A           │  │   release/1.0 ──► hot-fix   │  │   sandbox ──► expt-A        │
│        ╲                    │  │                             │  │                             │
│         ─► feat-B           │  │                             │  │                             │
│                             │  │                             │  │                             │
│   latest(F) = R8              │  │   latest(F) = R12             │  │   latest(F) = R3              │
└─────────────────────────────┘  └─────────────────────────────┘  └─────────────────────────────┘

  lock F on feat-A   →  caught-up check vs latest(F) = R8   in scope `main`
  lock F on hot-fix  →  caught-up check vs latest(F) = R12  in scope `release/1.0`
  lock F on expt-A   →  caught-up check vs latest(F) = R3   in scope `experiment`

  chains across scope boundaries are independent — an edit in any scope
  does not affect lockability of F in any other scope.
```

### Scopes (Goal 7)

A scope is a server-side entity identified by a stable `scope_id`, carrying a human-readable name and its own lifecycle (create, rename, delete) independent of any particular branch.

Branches are *members* of a scope via a `scope_id` field in branch metadata. A branch's scope is fixed at creation: a branch created from a parent inherits the parent's `scope_id` by default, or names a different `scope_id` explicitly. Stacked branches inherit transitively through their parent chain. This proposal treats scope assignment as immutable after branch creation; reassigning an existing branch into another scope is left to a follow-on (see Unresolved Questions).

Every repository has a **default scope** created automatically at repository initialization. New branches whose parent is in the default scope join it. A repository that never creates additional scopes operates entirely within the default — functionally identical to a global per-file chain.

Scopes are typically created to isolate long-lived release lines (each release line is a scope, taking backports without blocking main) or to wall off experimentation (a sandbox scope where unmergeable-file edits do not propagate constraints to production work). Scope creation is independent of branch creation: a user creates a scope, then creates one or more branches assigned to it.

A third use case is the **short-lived isolation scope**, complementary to the default-scope degenerate case. The user creates a fresh scope, assigns a single branch to it, does work that should not interact with any existing lock chains — no causality check against any other scope's latest entries — and merges to a target scope when done. At merge time, cross-scope merge semantics apply: the merge becomes a chain advance on the target scope, requiring a held lock on the target for each unmergeable file the merge touches, and resolving conflicts there. This is an explicit escape hatch for one-off work that has to happen without waiting for other in-flight locks or chains — urgent hotfixes, throwaway prototypes, parallel "what-if" iterations on the same asset — and accepts at-merge conflict resolution as the trade-off. Mechanically identical to other scopes; the difference is intent and lifecycle. So the design has two degenerate cases at opposite ends: a repository using only the default scope (one global chain, maximum coupling) and a repository spinning up a per-branch isolation scope (no shared chain at all, conflicts deferred to merge).

Scopes are stored in Lore's mutable store the same way branches are, using two paired `KeyType`s that mirror the existing `KeyType::BranchMetadata` (id → metadata) and `KeyType::BranchId` (name → id) pattern. One maps `scope_id → scope_metadata_hash` (the scope's existence and metadata, e.g. `KeyType::ScopeMetadata`); the other maps `scope_name → scope_id` (the human-readable name, also serving as the "is this scope active?" lookup, e.g. `KeyType::ScopeId`). The `scope_id` is the canonical identifier — what branches store in their metadata, what latest and lock entries key on. Names are purely a human-facing pointer to that id; renaming or detaching the name does not affect any other state.

Scope deletion is therefore a soft operation: removing the `scope_name → scope_id` mapping archives the scope (it disappears from `lore scope list` and name lookups fail), but the `scope_id`, its metadata, its latest entries, and any branch metadata that references it all persist. Branches assigned to an archived scope continue to function — their lock checks still resolve against latest entries keyed by the still-valid `scope_id`. Reinstating the name mapping (`lore scope restore <scope-id> <name>` or similar) brings the scope back into normal discovery. True purge (clearing the metadata and latest entries too) is the heavier operation and would still require no live latest entries and no member branches, but archive is the everyday lifecycle action and is fully reversible.

### Per-file latest pointer (Goal 1, Goal 2, Goal 7)

The server materializes one record per live `(scope_id, file_id)` pair — the **latest entry** — naming the chain's current tip and the content at it:

```
key:    Hash(scope_id, file_id)   // collision-resistant derivation, distributes evenly across keyspace
value:  latest_revision           // revision hash of the chain's current tip
        latest_content            // that revision's content hash for the file
```

Carrying the content alongside the revision is what keeps the caught-up check to a comparison; the revision is what the denial message and the audit trail name. Fields such as `latest_branch` and `updated_at` are intentionally absent — they're derivable from the revision and would only duplicate authoritative state, and UX surfaces resolve them lazily on the error path.

The entry count scales with **currently-live (scope, file) pairs**, not the all-time count: an entry is removed when the file is deleted in its scope and re-created on the next edit. It also doesn't multiply across scopes the way "live files × scopes" suggests, because a scope accrues an entry only for a file edited *in that scope* — a short-lived isolation scope touching five assets costs five entries whatever the repository holds.

**These entries live in the lock store rather than the mutable store.** An earlier draft of this proposal put them in `lore_storage::MutableStore` under a new `KeyType`, on the grounds that its native `Hash → Hash` shape fits. Specification found two reasons that doesn't work. Latest entries are never enumerated by key, so they need nothing the mutable store uniquely offers; and scope purge and capacity reporting both have to answer "every entry in this scope," which a hashed `(scope_id, file_id)` key makes unanswerable there — `MutableStore::list` takes a partition and a key type with no predicate. A store that can carry a scope index answers both, and the lock store already needs to be that store because an acquisition reads the latest entry and claims the lock together. Scopes, which *are* enumerated, do live in the mutable store.

### Lock acquisition (Goal 1, Goal 3)

For `lock F` on branch B given path P:

1. Determine `scope_id = scope_of(B)`. Branch → scope is a branch metadata lookup.
2. Resolve F in B.tip, determining `file_id` and F's content there. (Resolution failure returns "no such file in your branch" before any lock state changes.) The caller addresses the file by node identity rather than by path, so no path crosses the wire in either direction; a path is reconstructed when a person needs to read one.
3. Once `file_id` is known, insert an entry into the lock state for `(scope_id, file_id)`, failing with contention denial if an entry already exists. This is the actual mutex; holding it pins the latest value for the validation that follows — no concurrent writer can advance the latest without first obtaining this same lock.
4. After the lock insert succeeds, load the latest entry for `(scope_id, file_id)`.
5. Compare, and grant on either of two relations. **Caught up** — F's content at B.tip equals the entry's content. **Ahead** — it doesn't, but F's content history on B contains it, established by a walk bounded by the entry's revision number. An absent entry is first-edit semantics (post-delete resurrection, or the file's first edit in scope) and also grants. Otherwise B is **behind**: release the lock entry (it never had a chain-advancing claim) and deny with the latest revision and scope in the error payload.

Granting to an ahead branch is what Goal 1 asks for — it "contains every prior committed edit to F," which is the requirement, and caught-up is merely its zero-hop case. A branch gets ahead legitimately in several ways: through advisory policy, after an administrative force-release, from a concluded session's intermediate revisions, or when backfill seeds a chain from content the branch already descends from. Denying those protects nothing and blocks work sitting ahead of the protocol rather than behind it.

The ordering is deliberate: claiming the lock *before* reading the latest eliminates a race in which a concurrent lock-edit-commit-push cycle could complete between an early latest check and a later lock insert, leaving the requester holding a lock against a stale latest. With this ordering, the latest value observed in step 4 is stable for the rest of the operation because step 3 has already excluded every concurrent writer. The back-pointer read and the lock insert share only `file_id` as input, so they overlap rather than chain — the lock insert is dispatched as soon as the Merkle leaf yields `file_id`.

Cost: scope lookup (O(1) cached) + Merkle path resolution + max(back-pointer read, lock insert) + one mutable-store load. The validation-fails path adds one lock-entry delete; on the busy-file contended path that delete is unavoidable work and matches the rate of contention.

The above describes the **strict-default behavior**. Under advisory chain-enforcement policy the step-5 chain-behind denial becomes a warning and the lock is granted anyway; the audit trail records both the lock acquisition and the policy-allowed stale check. See Enforcement policy below.

### Multi-file acquisition (Goal 1, Goal 3, Goal 6)

Acquiring many locks at once is the common case — a folder rename, a multi-file commit, a prefab whose dependencies span the tree — and it has to be all-or-nothing where the caller needs it to be, without a ceiling on how many.

The acquisition is a **stream, and the stream is the transaction**. The caller opens it against a branch and a revision, sends entries, receives an outcome for each as it's decided, and ends with a commit that confirms every claim or a close that abandons them. Two properties follow, and both are the reason for the shape:

- **Every entry is judged against one view of the branch.** A request split into fixed-size batches is judged against as many reads of the branch tip as it has batches; one stream pins the tip once. No batching scheme recovers that, because the size limit is what forces the splitting.
- **There is no size limit to communicate.** Confirmation and abandonment address the whole transaction by its identifier rather than by a list of what was granted, so the cost of abandoning doesn't grow with the transaction and there's nothing to bound.

Atomicity becomes the caller's policy rather than a protocol mode: it decides what to do with the outcomes before committing, which expresses more than a fixed atomic-or-best-effort choice could — take a folder, but drop the meshes if the rig is contended.

Two costs come with it, and neither is hidden. Entries must be presented in a consistent key order, which the server verifies, because release-on-conflict between overlapping transactions is otherwise a livelock; under a single batch the server could have sorted them itself. And a large acquisition becomes one indivisible unit spanning whatever timeouts and restarts occur in its window, where a split request degraded gracefully.

### Why the revision-id check is sound

Lore's per-file-id back-pointer index records, for every revision in the graph, the most recent earlier revision in that revision's linear history that modified the file (by file_id) — implemented as the file-history-block walk in `lore-revision/src/file/history.rs`, surfaced by `find_last_modified_revision` in `lore-revision/src/revision.rs`. The chain invariant — only the lock-holder can produce the next chain link, and the lock-holder must already contain the prior tip — guarantees that the chain of modifications to F in a scope is linear. Therefore "B's most recent F-modifying revision" is well-defined and is a specific revision-id; comparing it to the latest's `latest_revision` answers chain-containment exactly. No bloom filters, no reachability bitmaps, no per-(branch, file) sparse matrix.

An alternative framing reaches the same answer: each chain advance produces a new tree entry for F (because the protocol-defined notion of modification is exactly what changes the tracked tree state), so the latest's `latest_content_hash` and B's content hash for F are equal iff `branch_last_mod_revision == latest_revision`. The revision-id check is the direct form; the content-hash check is an equivalent surface that is useful if Lore's back-pointer index is ever unavailable or if a caller needs to verify the equality without consulting the back-pointer.

### Chain advance at push

**Deferred chain advance.** The latest moves only at session conclusion — the push that releases the lock (or deletes the file). A locked session on F is the period from lock acquisition to lock release on F; it may span multiple pushes. Intermediate pushes from the lock-holder commit revisions to the holder's branch as normal but do **not** advance latest — the chain link for the session is held back until the push that unlocks. This is a deliberate protocol property: it gives the Administrative force-unlock semantic its clean revert behavior (latest still points at the prior concluded session), and it generates the "ahead of latest" state described below where another branch in the scope can have synced an intermediate revision and still legitimately get a chain-behind denial.

When B pushes a commit C that modifies a set of unmergeable files (identified by `file_id`, not path) `{F_1, F_2, …, F_m}`, with `scope_id = scope_of(B)`:

The push carries an `unlock_files` parameter naming which of the modified files to unlock after the chain advance — either an explicit list (subset of `{F_1, …, F_m}`), the sentinel `all` meaning "every file this push modifies," or an empty list to keep every lock held. `all` is the default and matches the typical edit-commit-push-done workflow; specifying a subset is the opt-in for sessions that hold a lock across multiple pushes against the same file (so the user can keep the chain pinned for the next commit without re-acquiring).

For each `F_i` in `{F_1, …, F_m}`, the server runs the following in parallel — each `F_i`'s per-file work is independent of every other `F_j`:

1. Verify B currently holds the lock on `(scope_id, F_i)` (fresh read).
2. **Latest update (conditional on session conclusion).** If this push concludes the locked session for `F_i` — that is, `F_i` is named in `unlock_files` (including via the `all` sentinel) *or* the push deletes `F_i` — write the latest: `store(repository, Hash(scope_id, F_i), C, KeyType::UnmergeableLatest)`. For a delete, the value written is `Hash::default()` (the null hash), which removes the entry per the mutable store's contract — matching the "delete removes the latest" semantics described below. If the push modifies `F_i` but keeps the lock and isn't deleting, the latest is *not* updated by this push; the chain advance is deferred until the locked session is concluded by a later push that unlocks (or deletes). Intermediate revisions still sit in the file's normal revision history, but they are not chain tips.
3. **Lock release (conditional).** If `F_i` is named in `unlock_files`, remove the corresponding lock entry. Otherwise the lock remains held; the next commit-and-push from the same branch can advance the chain again without re-acquiring.

No CAS on the latest is needed — the lock is the sole concurrency control on latest writes. As an inexpensive belt-and-bracers check, the server may verify that C's back-pointer for `F_i` (the prior F-modifying revision in C's history) equals the current latest value before overwriting it; mismatch indicates either a lock-correctness bug or a client lying about its parent, and should fail loudly.

**Interaction with cross-branch back-pointer checks during a locked session.** Because latest advance is deferred until the session concludes, a different branch in the same scope that syncs an *intermediate* revision from the locked session has, in its history, an F-modifying revision more recent than the (unchanged) latest. Its `branch_last_mod_revision` is a descendant of `latest_revision`, not equal to it, so the standard lock check denies with "chain-behind" even though the requester is content-wise ahead of latest. This is the correct protocol behavior: only the lock-holder may advance the chain, and the chain hasn't advanced yet. In practice the requester is already blocked by contention denial (the lock entry exists), so they retry after the session ends; by then latest has advanced to the session's final commit, and back-pointer comparison resolves normally.

Cross-key atomicity (so the push either fully succeeds across all `F_i`, or has no effect on any of them) uses the same distributed-commit mechanism Lore already employs for branch advance. Per-file work within that umbrella parallelizes naturally — the verify, conditional latest write, and conditional unlock for each `F_i` are independent of every other `F_j`'s work, so the per-file dimension fans out across the storage layer.

The above describes the **strict-default behavior**. Under advisory lock-enforcement policy, step 1's lock verification becomes a warning if the lock isn't held and the push proceeds anyway; under advisory chain-enforcement policy, the belt-and-suspenders parent-mismatch check becomes a warning instead of a hard fail. Both transitions are audit-logged. See Enforcement policy below.

### Optional: collapsing quiescent latest entries

When all branches in a scope share the same back-pointer answer for F — i.e., every branch in the scope has observed the same most-recent F-modifying revision — the chain is *quiescent*. The latest entry adds no information at that point: any lock attempt would succeed with its back-pointer matching the latest, write the same chain forward, and update the latest in place. Collapsing the latest back to "no entry" is safe and reclaims storage; the next commit anywhere in the scope re-establishes the latest via the existing first-edit semantics.

Mechanism:

1. Detect consensus for `(scope_id, file_id)`: for every branch B in the scope, gather `back_pointer(B.tip, file_id)`. If all equal `latest_revision`, the chain is quiescent.
2. `compare_and_swap(latest_key, latest_revision, Hash::default())` — collapse the entry. The CAS predicate guards the case where a push advanced the latest between observation and write; on CAS failure, defer.

Safety follows from the lock table being the actual mutex on chain advance, not the latest entry. A deleted latest is mechanically indistinguishable from a never-existed latest: first-edit semantics still serialize concurrent acquirers through the lock-table insert-if-not-exists, and the first push after collapse re-establishes the latest. A lock holder racing with the collapse is also benign — the CAS can null the latest while the lock is held, and the holder's eventual `store(latest_key, new_revision)` simply overwrites null with the new revision (no CAS needed on the push side because the lock is the sole legitimate writer).

Trigger strategy is deferred to the design phase (see Unresolved Questions): server-side periodic sweep, event-driven hook on likely-consensus moments (post-merge, branch deletion, syncing push), or lazy detection at next lock attempt are all viable; each has different freshness/cost trade-offs.

This optimization is layered, not load-bearing: the proposal is correct without it. Steady-state benefit is that storage tracks live *divergent* (scope, file) pairs rather than every live (scope, file) pair — usually a small fraction, since most files spend most of their time in consensus.

### Lock state — required contract (Goal 1)

What this LEP mandates is the contract a lock store must satisfy to implement the protocol. Storage choice — the existing `lore_revision::lock::LockStore` trait suitably re-keyed, a new mutable-store-backed implementation, or any other store — is downstream of this LEP.

**Key shape.** Locks are identified by `(scope_id, file_id)`. The chain invariant depends on two branches in the same scope contending on *the same* key when they want to lock the same file. Branch is *not* part of the key — it appears in the value (below). This is the structural reason the existing `LockStore` trait, which keys on `(repository, branch, hash)` with branch in the key, cannot be reused without adaptation.

**Required operations.**

- **Atomic batch insert-if-not-exists** across a set of `(scope_id, file_id)` keys, for atomic-mode batch acquisition. Either every key succeeds or none does; on partial conflict the implementation releases its partial acquisitions and reports the conflicting keys.
- **Independent batch per-key insert-if-not-exists** across a set of keys, for best-effort-mode batch acquisition. Per-key outcomes, no cross-key dependency.
- **Conditional batch release** of a set of keys, authorized by the pushing branch (which must match the current holder).
- **Holder read** for a given key, returning the current holder's `branch_id` or absent. Used by push-time verification and by lock-denial messaging.

**Value contract.** The entry must associate the held key with the holder's `branch_id` (enough for the two checks above: push-time identity-match and lock-denial holder display). The chain protocol depends on nothing else in the value; any further fields (acquired-at timestamp, lease expiry, sidecar metadata) are implementation choice.

**Concurrency contract.** Insert and release operations must be atomic with respect to each other and to themselves; the protocol relies on the lock being a true mutex for the latest it pins.

**Out of scope.** Lease semantics (heartbeats, expiry, zombie cleanup), on-disk representation, the query surface beyond what the operations above require, and any rich-predicate lookups are not part of the contract — they are operational and implementation concerns that do not change the chain protocol.

### Enforcement policy (Goal 9)

The two primitives this proposal introduces are informational at the protocol level: the **lock entry** carries the signal "this file *is* being modified by branch B right now," and the **chain latest** carries the signal "this file *has* been modified up through revision R; your branch is at R'." How strictly the server gates operations on those signals is a separate concern — a repository policy, not a protocol property. The proposal specifies the signals and the strict-default behavior; the exact policy mechanism (per-repository setting, per-scope override, branch-protection-style rules, or something else) is deferred to a follow-on.

A minimal viable mechanism is a per-axis flag in repository metadata that the server reads on each request — mechanically straightforward and adequate for the strict-by-default proposition this LEP makes. Richer alternatives (per-scope override, branch-protection-style rules) are valid extensions but not protocol-blocking; the choice is safe to defer alongside the rest of the repository-administration tooling.

The protocol exposes two independent strictness axes:

- **Lock enforcement policy.** Under **lock-strict** (the default), the push handler rejects a push that *collides*: another branch holds the file, or the content didn't start from the chain's latest. A push holding no lock that turns out to be a direct successor is accepted, because refusing it would protect nothing. Lock acquisition still serializes at the lock store regardless of policy. Under **lock-advisory**, the push handler emits a warning and audit-logs the push but allows it to proceed. The lock entry continues to surface "who is editing" for coordination; what advisory mode disables is the server's refusal to accept pushes from non-holders.
- **Chain enforcement policy.** Under **chain-strict** (the default), lock acquisition denies if the requester's `branch_last_mod_revision` is not equal to `latest_revision` (chain-behind denial), and push validation rejects pushes whose parent for F does not match the current latest. Under **chain-advisory**, both points emit warnings and audit-log but allow the operation. The chain latest continues to surface "is your view current" for staleness display; what advisory mode disables is the server's refusal to advance a divergent chain.

The two axes combine independently: a repository can run lock-strict + chain-strict (the safety-maximizing default), lock-strict + chain-advisory (must respect in-flight sessions, but stale-acquire is allowed), lock-advisory + chain-strict (no enforcement against off-protocol pushes, but chain integrity is still gated), or both advisory (the primitives become pure signals; closest to Git LFS's posture today).

**The axes also differ in who they can apply to, because they ask for different kinds of cooperation.** Lock enforcement asks for a convention — take a lock before changing an unmergeable file — which a client can follow without knowing anything about chains, scopes, or causality, and which the existing lock API already expresses. Chain enforcement asks for participation: receiving a denial, understanding it, merging, and retrying. A client that predates this proposal can do the first and not the second. So lock enforcement applies to every client, and chain enforcement applies only to those that can hold up their end. The residual gap — a client legitimately holding a lock while behind, and publishing a divergent edit — is exactly today's behavior rather than a regression, it closes as clients adopt the protocol, and every instance is recorded on the chain entry so the cost of it becomes measurable rather than assumed. Every operation that proceeded under advisory policy is audit-logged with the same fields the strict-denial path would have emitted, plus the policy that allowed it — so operators can see what would have been denied under stricter policy and how often the relaxed policy is being relied upon.

Advisory mode preserves the protocol's chain-tracking machinery as a source of truth: the latest still advances, the lock entries still come and go, and the audit trail still records who did what. What changes is only the server's response to off-protocol intent — *deny* under strict, *warn and record* under advisory. Clients cannot bypass the protocol either way; they can only operate within the strictness the repository has set.

### Administrative force-unlock

The protocol exposes an admin-only operation that forcibly releases a held lock without going through the normal commit-and-push cycle. This is the operational escape hatch for stuck or abandoned sessions — a workstation that crashed mid-edit, an account that left the team, an automated process that took a lock and never released it.

The mechanism is straightforward because the latest is only updated when the locked session is *concluded* by a push (Chain advance at push, step 2): force-releasing a held lock simply deletes the lock entry. The latest pointer is not touched. Note that this makes force-unlock a lock-release operation rather than a revert: the ex-holder's branch is ahead of the chain, so it may re-acquire and conclude the very session that was force-released. That's the wanted behavior for the case force-unlock exists for — a zombie lock from a crashed workstation — and it means deliberate abandonment needs a different instrument, such as branch protection. Any intermediate revisions the holder pushed during the session — commits that modified the file but did not include it in `unlock_files` — remain in the holder's branch revision history as ordinary commits, but the chain latest still points at the revision from the *previous* concluded session. From the chain's perspective the operation is equivalent to reverting the file to its prior latest: any client that locks F next sees the chain content as of the last concluded session, not the abandoned in-flight content.

The force-released branch is now in an "ahead of latest" state for that file: its back-pointer points at an orphaned intermediate revision that is no longer the chain tip. Subsequent lock attempts from that branch are denied with the chain-behind error pointing at the older latest; the error message can flag a force-release origin so the user understands their in-flight work was abandoned. Recovery options:

- **Accept the revert.** Merge or sync the new (force-released) state of F into the branch and replay the intended edits under a fresh lock.
- **Restore the abandoned work.** An admin who wants to preserve the holder's in-flight commits can acquire a fresh lock from the holder's branch (since the branch already contains a more-recent F-modifying revision than latest) and push a chain advance that establishes the abandoned tip as the new latest — closing the session that was force-released open.

Force-unlock authorization shares the `--supersede` model used for scope-administration overrides (see Unresolved Questions for the authorization story). Every force-unlock event is recorded in the latest-advance audit trail (see Observability under Non-Functional Considerations) with admin identity, target `(scope_id, file_id)`, prior holder `branch_id`, and the latest revision unchanged by the operation — so the operation is traceable even though it bypasses the normal commit-and-push flow.

The same primitive answers two operational needs: it cleans up zombie locks (the standard use case), and it provides an explicit "abandon this session and revert to last-concluded state" mechanism for situations where the lock holder's work should not enter the chain at all.

### Identity by file_id and scope_id (Goal 5, Goal 7)

All cross-branch state — latest entries, lock entries, caches — keys on `Hash(scope_id, file_id)`, never on path. Rename and move don't disturb file identity. B1 may see F at `assets/old/foo.uasset` and B2 at `assets/new/foo.uasset`; if both are in the same scope, both derive the same key and contend against the same latest value; if in different scopes, they derive different keys and sit on independent chains. Scope lookup is a single per-branch cached value.

**A lock belongs to the repository that owns the file, not the one the caller reached it through.** Lore links repositories into one another, and a linked repository can be mounted at more than one path and by more than one parent. Identifying a lock by the mount path would give the same file a different identity per mount, so two people editing it would never contend — and today's path-keyed locks do exactly that, in the parent's partition rather than the owning one. Keying on `file_id` removes the problem by construction: the identifier is allocated in the owning repository and travels with the node, so every mount derives the same key. Crossing a link means crossing into that repository's state, and the scope resolves there too, against the linked repository's own branches.

### Unmergeability

The server decides whether a file is unmergeable on every push, so the decision has to be cheap and it has to be defined here rather than deferred. Two sources answer it, and the explicit one wins.

**Repository policy, by file type.** A repository declares which extensions are unmergeable. Matching against a file's own name costs a single lookup on a record the push already holds, where a path-pattern policy would force a full path reconstruction per candidate file and put a cost proportional to tree depth on the hot path.

**A per-file override**, carried on the node record, for a file which is unmergeable although its type isn't, or the reverse. A spare field on the record holds it, and its unset state — which every existing repository has — means "defer to the repository policy," so no migration and no format version change.

The override has a limit worth naming: a client that predates it rewrites node records without knowing the field carries meaning, so it can clear an override another client set. Repository policy is therefore the dependable half until clients converge, and the override is the exception rather than the mechanism.

### Content-only chain-link semantics (Goal 4)

Only changes to an unmergeable file's *content* (the BLAKE3 hash on its leaf node in the Merkle tree) advance the chain for the scope the push happens in. Metadata-only edits (mode, timestamps, extended attributes) and path-of-record changes (rename, move) produce new tree entries but are *not* chain-advancing — they do not require a lock and do not pass through the chain protocol. Cross-branch path divergence is resolved at merge time via Lore's normal tree-merge, the same way it works for mergeable files: `file_id` is stable across renames, so a merge sees the same file at two paths and picks one, content-consistency already enforced by the chain on the orthogonal content axis. Copy creates a new `file_id` and a new chain (in the active scope); F's chain in any scope is untouched.

Delete is a chain-terminating operation in its scope that **removes the latest entry** for `Hash(scope_id, file_id)` rather than leaving a terminal entry behind — mechanically, the push writes the null hash to that key, which the mutable store treats as a key removal. A subsequent edit in the same scope on any branch where F still exists (branched from before the delete) finds no latest entry, gets the lock unconditionally (first-edit semantics), and re-establishes the latest on commit. The lock-state mutex serializes the first-edit-after-delete window: only one branch in the scope can hold the lock at a time, so the first to commit fixes the new latest, and any further concurrent resurrection attempts go through the normal successor-only check against that new latest.

Latest-entry removal happens at push time when the pushed commit deletes F — not when a branch tip "no longer has F" by other means, which is meaningless under free branching. A feature branch that pushes a delete of F removes the latest in its scope even if other branches in the same scope still have F. The theoretical safety property: a delete-vs-edit conflict between two branches always has a no-loss resolution because the user can elect to keep the edit (the delete carries no content state to discard, only an intent to remove). Resolving in favor of the edit preserves all work; resolving in favor of the delete is a deliberate user choice to discard work. Either way, the conflict has a defined outcome and no work is lost without explicit user direction. Deletions and resurrections in one scope are independent of any other scope's chain — F may be deleted on a release scope while remaining alive (and editable) on main's scope.

### Cross-scope merges

A merge that pulls commits from a branch in scope A into a branch in scope B is the one operation where scope boundaries are intentionally crossed. When the merge brings in commits that touch an unmergeable file F, the merge produces a new tree entry for F in scope B that is **not** derived from scope B's existing chain. The proposal treats this as a chain advance in scope B requiring a held lock on F in scope B (i.e. the `(scope_id, file_id)` key for the target scope) for the duration of the merge commit; the source-scope chain is not consulted. The cross-scope merge is a fresh chain link in the target scope, regardless of what F's history looked like in the source scope.

This preserves the per-scope chain invariant (every advance is a successor of the prior latest in its own scope) and keeps cross-scope semantics straightforward: scopes never share chain state, only branch DAG ancestry. The cost is that backporting an unmergeable-file fix from main to a release scope requires holding the lock in the release scope (and the merger choosing to apply the change there), exactly as a fresh edit on the release scope would — which is the correct user-facing model.

### Lock-denial error surface (Goal 6)

The denial response carries `(latest_revision, scope_id, suggested_action: "sync"|"merge", source_branch_hint)`. The CLI presents:

```
error: cannot acquire lock on assets/hero.uasset — branch is behind on this file in scope 'main'
  latest revision: 9f3a...c12d (created on feature/lighting, 2026-05-14)
  scope:        main
  suggested:    lore sync   # if the latest is reachable on this branch's upstream
                lore branch merge feature/lighting
```

When a lock fails because the branch is in a different scope than the requester expects (e.g., the user thought they were on a branch assigned to the release scope but it inherited the default scope from its parent), the error names the actual scope explicitly so the user can correct the branch choice or re-target.

`source_branch_hint` and scope display name are derived from revision and scope metadata, not from denormalized fields in the latest entry.

## Compatibility

- **Wire format** — N/A. No changes to existing message encodings, framings, or content-address derivations.
- **Client/server protocols** — Additive. A new versioned lock service alongside the existing one, streaming rather than unary for acquisition, release, and status, since [multi-file acquisition](#multi-file-acquisition-goal-1-goal-3-goal-6) needs one consistent view of the branch and no size ceiling. The existing lock service keeps its wire shape permanently and is served from the same state, so both surfaces see one set of locks. A client discovers the new service through the existing environment configuration and falls back to the old one when a server doesn't advertise it, so an old client against a new server and a new client against an old server both keep working with no negotiation to add.

  Push gains a validation step for unmergeable-file latest and lock state (failure modes: collision with another holder, content that isn't a successor) and a parameter controlling which locks the push releases. **That parameter defaults to releasing nothing**, which is what an old client's push means and what a new client's push means without an explicit choice — so push behavior is identical across every client version.
- **On-disk format** — N/A for repository data. Scope metadata and scope-name lookup add two `KeyType`s to Lore's existing mutable store (`KeyType::ScopeMetadata`, `KeyType::ScopeId`), mirroring the existing branch storage pattern; scopes go there because listing them is what the mutable store uniquely offers. Latest entries and lock state live together in a store satisfying the lock contract specified in Proposed Design — the existing `LockStore` trait suitably re-keyed, a mutable-store-backed implementation, or another conforming store; the choice is downstream of this LEP, and a self-hosted deployment can use the mutable store for both rather than run more storage. One further on-disk change: a spare field on the tree's node record carries the [per-file unmergeable override](#unmergeability), which reads as unset in every existing repository, so no format version moves. The repository's Merkle tree layout, fragment encoding, branch tips, and revision-record format are otherwise unchanged.
- **CLI and public API** — additions and behavior changes by command family:
  - **`lore lock`**
    - `lore lock acquire` gains a new failure mode `LORE_ERROR_CODE_LOCK_CHAIN_BEHIND` when the branch is behind on the file in its scope, with error payload identifying the latest revision and `scope_id`. A branch which is *ahead* gets granted, not denied.
    - `lore lock release` is unchanged.
    - `lore push` gains a flag naming which locks to release; without it, a push releases nothing.
  - **`lore scope` (new subcommand family)**
    - `lore scope create <name>` — creates a scope, returns the new `scope_id`.
    - `lore scope list` — lists active scopes; `--all` includes archived.
    - `lore scope info <scope-id|name>` — displays scope metadata.
    - `lore scope rename <scope-id> <new-name>` — changes the display name.
    - `lore scope delete <scope-id>` — soft delete; drops the `scope_name → scope_id` mapping. `scope_id`, metadata, latest entries, and branch references persist.
    - `lore scope restore <scope-id> <name>` — reinstates a name mapping for an archived scope.
    - `lore scope purge <scope-id>` — hard delete; clears metadata and latest entries too. Refused while any live latest entries or branch references remain.
  - **`lore branch`**
    - `lore branch create` gains a `--scope <scope-id>` flag to assign the new branch into an existing scope; without the flag the new branch inherits its parent's scope.
    - `lore branch info` output includes the branch's `scope_id` and the scope's display name.
    - `lore branch delete` gains a precondition: refused when the branch is the current latest-holder for any unmergeable file still alive elsewhere in its scope; requires `--supersede` or a forward-merge.
  - **Existing scripts** that only acquire and release locks in a single-scope repository continue to work. A script that edits unmergeable files *without* locking also continues to work, as long as it stays current — under lock-strict what gets refused is a collision, not the absence of a lock. What stops working is an edit made from stale content, or one racing another editor, which are the cases that previously destroyed work silently at merge.
  - **Locks become per-scope rather than per-branch**, so two branches in one scope that could each hold the same path independently now contend. That's an increase in protection rather than a new refusal, and it's the one semantic change that reaches clients which opted into nothing.

## Non-Functional Considerations

- **Concurrency** — The exclusive lock is the sole mutex on latest writes; no CAS is required on the latest store. Lock acquisition uses the lock store's atomic insert-if-not-exists primitive. Concurrent lock attempts on the same `(scope_id, file_id)` serialize at the lock store. Concurrent push validations across files parallelize naturally — each latest key is independent. Multi-file commits inherit Lore's existing cross-key atomicity mechanism for branch advance.
- **Memory** — Per-entry latest state is one `Hash` value (~32 bytes payload in the mutable store). Lock-entry size is implementation-dependent but small — bounded by holder `branch_id` plus any sidecar lease metadata. Worst-case latest storage is `live unmergeable files × scopes`; with a typical ~10 scopes and 100M live files, well within the mutable store's normal operating range. Practical sizing is far lower because most files exist in only a subset of scopes, and with the quiescent-latest collapse optimization steady-state sizing is live *divergent* (scope, file) pairs — usually a small fraction of all live (scope, file) pairs, since most files spend most of their time in consensus. Lock check is constant memory per request; Merkle traversal (which also yields the back-pointer) is O(path depth). No structures scale with `branches × files`.
- **Statelessness** — Latest entries live in Lore's existing mutable store under a new `KeyType` (`KeyType::UnmergeableLatest`) and are durable. Lock state lives wherever the implementing lock store places it; the lock contract requires durability sufficient to honor lease semantics across process restart, but does not constrain the storage layer beyond that. Clients hold no new state.
- **Determinism** — Latest advance is a deterministic function of `(lock holder, push contents)`. Same sequence of acquisitions and pushes yields the same latest sequence. The lock check is a pure function of `(stored latest value, branch's back-pointer answer for the file_id)`.
- **Observability** — The proposal introduces new server-side state (latest entries, lock entries, scope entries) and new failure modes (causality denial, wrong-scope denial, lock contention, lease eviction) that operators need to monitor and debug. The design phase specifies a metrics surface covering at minimum: lock-acquisition success rate and denial rate broken down by reason (chain-behind, contention, not-found-in-branch, wrong-scope); latest-advance rate per scope; lease-eviction events; quiescent-latest collapse events (if the optimization is enabled); scope lifecycle events (create, rename, delete/archive, restore, purge); and mutable-store operation latencies for the new `KeyType`s. An append-only audit trail for latest advances — recording `(revision, scope_id, file_id, branch_id, holder_identity, timestamp)` for each advance — is the operational counterpart to the durability of the latest entries themselves; without it, "who advanced this chain" turns into archeology against revision metadata.

## Migration Plan

Specification expanded this into a nine-phase plan; what follows is the shape it settled on, with the phase-by-phase compatibility matrix and gates living in the implementation plan.

**Nothing changes behavior on deploy.** Each phase reaches a live fleet as a rolling deploy, so two server versions run concurrently against the same state for its duration. A phase whose behavior changes when its binary lands behaves two ways at once — and for the phase that redirects the existing lock service onto the new state, that means one server granting a lock the other can't see, silently. So code ships inert behind a flag, the fleet reaches one version, and behavior is enabled by a fleet-wide configuration flip. Rollback is the reverse flip rather than a redeploy, which is what makes it fast enough to use.

**Storage and policy first, enforcement last.** Scope entities, the unmergeable declaration, and the lock store all land and sit unused. The new lock service goes live next, still advisory on both axes, so the checks run and report while nothing is refused — which is what tells an operator the denial rate they'd be signing up for. The existing lock service is then redirected onto the same state, so both surfaces see one set of locks. Only then is enforcement enabled, per repository.

**Backfill runs after enforcement, not before.** A file with no latest entry takes first-edit semantics and grants, so an unbackfilled repository is permissive rather than broken and the protocol tightens as backfill progresses. Running it first would mean a long window with enforcement live and untested against real entries.

**Divergence is surveyed before it's enforced.** Free branching without a chain has already produced unmergeable files sitting at different content across branches of what becomes one scope. Backfill can only choose one of those as the chain's latest, and every branch that doesn't contain the chosen content is behind from that moment. The [ahead relation](#lock-acquisition-goal-1-goal-3) narrows this — a branch that merged the winning content at some point is unaffected — but what remains is real, it's inherited rather than created by this proposal, and it's the largest adoption risk in the rollout. A read-only survey reports the exact set before backfill writes anything.

**Two things the original sketch of this plan got wrong.** It called for stamping every existing branch with a scope id in a bulk write; deriving the default scope's identifier from the repository makes that unnecessary, since an absent scope on a branch already resolves to the right answer. And it placed latest entries in the mutable store, which [Per-file latest pointer](#per-file-latest-pointer-goal-1-goal-2-goal-7) revises.

**Rollback.** Disable enforcement and clients get pre-existing lock semantics while latest entries continue to be written but not consulted; scope declarations remain but have no effect. The observable signal is a sustained rate of chain-behind denials that doesn't correspond to legitimately stale branches, which indicates a backfill or chain-state error.

## Security Considerations

The new mechanism does not change Lore's trust boundary. Latest writes are server-side, gated by server-verified lock ownership; clients never directly mutate latest entries. Lock acquisition flows through existing authentication and branch ACLs — a caller cannot lock a file on a branch they could not commit to today.

A malicious caller cannot construct a latest that bypasses content integrity: latest entries reference revisions that themselves go through the existing content-addressed validation. A malicious peer cannot poison latest state for another branch because they cannot acquire the lock without satisfying the causal check. The worst attack a permitted-but-malicious user can do is hold a lock and refuse to release — which is the same denial-of-service the existing lock primitive already permits, handled by the same lease-and-force-release mechanism.

## Privacy Considerations

Latest entries hold `(Hash(scope_id, file_id), latest_revision)` — no user identity, no path. Lock entries (per the lock contract in Proposed Design) associate `(scope_id, file_id)` with the holder's `branch_id`; no user identity directly, and the human holder is derivable via the branch's existing metadata, which is already visible through existing lock-query mechanisms. No new user-identifiable data is collected, persisted, or made visible beyond what existing lock state already exposes. Deletion and redaction follow the existing revision and lock policies.

## Risks and Assumptions

**Assumptions.** Specification checked each of these against the tree. Two were invalidated and the design changed accordingly; the findings are recorded inline rather than the assumptions being quietly deleted.


- **Assumption:** Lore's per-file-id back-pointer index (file-history blocks; `lore-revision/src/file/history.rs`; surfaced via `find_last_modified_revision` in `lore-revision/src/revision.rs`) resolves "most recent F-modifying revision in B's linear history" in roughly the same cost as a Merkle leaf traversal — and is updated atomically as part of every revision that modifies the file. *Invalidated if:* the back-pointer requires a separate index lookup with materially different cost, or if it lags revision creation in a way that exposes stale answers to the lock check. **Holds, with a caveat found in specification:** the helper is the right primitive precisely because the file-history weave lags one commit behind a branch tip, so the tip's own delta has to be consulted before the back-pointer. Reading the pointer alone reports one revision too far back on a branch that just committed. The helper is private and shaped for merge, so it needs opening up rather than calling.
- **Assumption:** Lore's per-file-id back-pointer can be queried specifically for *content-changing* revisions (revisions where the file's BLAKE3 content hash changed), distinct from tree-entry changes that only updated metadata or path-of-record. *Invalidated if:* the back-pointer cannot distinguish content updates from metadata- or rename-only changes — in that case the protocol must filter at lookup time (skipping back-pointer hits whose tree entries match the previous content hash) or treat the divergence as a fallback path. **Invalidated:** the commit path advances the pointer for every entry in a revision's delta whatever its action, so a rename advances it exactly as a content edit does. The proposal takes the filtering fallback it anticipated, which is why the caught-up check compares content and keeps revision identity as the secondary form.
- **Assumption:** `file_id` is stable across rename, move, and content edit, and is unique per file across the repository. *Invalidated if:* a rename or move produces a new `file_id`, or `file_id` is recycled after deletion, in which case identity has to be re-keyed.
- **Assumption:** Per-file, per-scope lock-acquisition rate is bounded by human or coordinated automation rates (≤ a handful per second per file per scope). *Invalidated if:* an uncoordinated automated workload attempts thousands of lock cycles per second on one (scope, file) pair, making the single-key write a hot spot in the mutable store.
- **Assumption:** Lore already has a distributed-commit mechanism that can advance multiple branch-tip-like rows atomically. *Invalidated if:* multi-file unmergeable pushes have to invent a new atomicity protocol. **Invalidated:** branch advance is a single-key compare-and-swap and the mutable store offers no transaction. Rather than inventing one, the design records the concluding revision on the lock entry before the branch advances, making the chain write a forward-only replay from durable state that the pusher's retry, a later acquisition, or a lease reaper can each complete. Cross-key atomicity turns out not to be needed.
- **Assumption:** The number of scopes per repository stays modest (single digits to low tens). *Invalidated if:* workflows demand hundreds or thousands of scopes per repo, in which case per-scope entry count and scope-lookup caching strategies need re-examination. **Doesn't need to hold:** nothing on a request path scales with the scope count — a branch belongs to one scope, and lookup is one cached read — so the design sets no limit. Storage doesn't multiply either, since a scope holds an entry only for a file edited in it, which is what makes a short-lived isolation scope cheap enough to reach for freely.

**Risks**

- **Risk:** Deletion of a Lore branch that holds latest for many files (the latest revision becomes unreachable from any surviving branch in the scope, while the file itself remains alive on other branches at older chain links) leaves locks unacquirable until resolved. *Mitigation:* refuse branch deletion when the branch holds latest for any unmergeable file that is still alive elsewhere in its scope; require `--supersede` (admin or explicit) or a forward-merge to clear the latests first. (File-delete cases do not trigger this risk because the latest entry is simply removed via writing the null hash.)
- **Risk:** A hard scope `purge` while live latest entries or member branches still reference the scope_id would orphan chain state and dangle branch references. *Mitigation:* `lore scope purge` is refused under either precondition; ordinary `lore scope delete` is a soft operation (archive) that drops only the name → id mapping, so chain state and branch references stay intact and the action is reversible via `lore scope restore`. Hard purge is the heavier operation and is only invoked when the scope is truly empty of live state.
- **Risk:** Users misplace work in the wrong scope (branch off main when they meant to branch off a release scope, or vice versa) and discover it only when a lock denial points at an unexpected scope. *Mitigation:* `lore branch info` surfaces scope membership prominently; `lore branch create` confirms the inherited scope; lock-denial errors name the actual scope so the mismatch is legible.
- **Risk:** Long-held locks on hot unmergeable files become a productivity bottleneck within a scope. *Mitigation:* lease-with-heartbeat plus admin force-release path (orthogonal lock-state concern, addressed by existing operational tooling). Across scopes the risk does not amplify — independent scopes do not contend.
- **Risk:** Phase 2 backfill races with concurrent edits, producing stale or wrong latest entries. *Mitigation:* backfill writes use the mutable store's `compare_and_swap` with an expected value of the null hash (entry absent); concurrent edits during backfill always win, producing a current latest.
- **Risk:** Stale or abandoned branches in a scope keep the quiescent-latest collapse optimization from ever firing — the consensus check fails because at least one (forgotten) branch lags behind. *Mitigation:* the optimization's branch-set should be filtered to "active" branches (modified within some window, or holding a tip that isn't reachable from another live branch); abandoned branches are an orthogonal cleanup concern.
- **Risk:** Repository policy drifts toward advisory enforcement habitually, defeating the protocol's safety guarantees through configuration rather than through code. *Mitigation:* operator-facing dashboards built on the audit trail surface advisory-allowed events alongside what would have been denied under strict policy, making the cost of the policy choice visible; the policy-setting mechanism (out of scope for this LEP) is expected to gate the relaxation behind explicit configuration rather than a default-on switch.

## Drawbacks

- Every live (unmergeable file, scope) pair gains always-on server-side state that did not previously exist, even ones rarely edited; storage and operational tooling have to cover them indefinitely.
- Adds a new lock failure mode (causality denial) distinct from contention denial; users need to learn the difference between "someone else holds the lock," "you're behind on this file in this scope," and "you're in the wrong scope."
- Scope is a new concept users have to model whenever they go beyond the default — scope creation, branch assignment, and cross-scope merges all become explicit decisions.
- Downstream tooling that wants to rely on strict chain semantics — e.g. CI checks that assume the chain latest is the authoritative latest revision — must either depend on the per-repository strictness policy or treat the chain as best-effort, complicating any integration that prefers not to be policy-aware.

## Alternatives Considered

### Per-branch ephemeral locks (status quo)

Keep existing exclusive locks, scoped per branch or globally, without a causality check. Trust users to merge before locking.

*Rejected because:* this is exactly the model Motivation argues is structurally incomplete. The harm of a missed merge is not visible until merge time, and the cost at that point — discarded work on an unmergeable file — is precisely what the lock was supposed to prevent.

### Auto-merge on lock denial

When the lock check fails, the server merges the latest revision into the requester's branch automatically before granting the lock.

*Rejected because:* merging a revision brings in not just the F edit but the source branch's entire causal closure up to that point, including changes unrelated to F. The user must decide whether and how to accept those changes; this proposal makes the decision explicit by surfacing the denial instead of silently doing a non-trivial merge on behalf of the lock requester.

### Single-trunk model for unmergeable files

Disallow editing unmergeable files outside a designated trunk (e.g., `main`). All edits must happen on trunk.

*Rejected because:* it forces every artist or pipeline that touches an unmergeable file onto a single branch, eliminating the value of Lore's free branching for the workflows that most need it (long-running feature branches, parallel content streams, experimental work).

### Single global chain per file (no scopes)

Keep one chain per file across the entire repository. Lock check is always against the global latest; release branches, experimentation, and main all share one chain.

*Rejected because:* a single global chain over-couples independent lines of work — a backport on a release branch blocks every editor on main, an experimental edit on a sandbox branch freezes the same asset for everyone else. The proposed design retains this model as the degenerate case (a repository that only uses the default scope operates exactly this way) while letting users opt into partitioning when their workflow needs it.

### Scope tied to specific branches (scope-as-branch-attribute)

Tie scope identity to a designated "scope branch" — the branch's identity *is* the scope. Membership is derived from descent: any branch derived from a scope branch joins its scope. No separate scope entity exists.

*Rejected because:* scope lifecycle becomes coupled to one specific branch's lifecycle. Deleting the anchor branch needs ad-hoc machinery to preserve or transfer scope identity; renaming a scope branch either loses chain history (if scope id was the branch name) or requires a separate stable id alongside the branch — at which point a decoupled scope entity is already implicit. Treating scope as a first-class entity removes these awkward dependencies: branches come and go, scopes persist as long as their chain state and branch references do.

### Materialized (branch × file) up-to-date matrix

Precompute, for every (branch, unmergeable file) pair, whether the branch is at latest. Lock check is a single keyed read.

*Rejected because:* at 10K branches × tens of millions of files, the matrix (even sparse) has 10¹¹-scale write rates and enormous invalidation fan-out on each push. The Merkle equality check delivers the same answer in O(path depth) without materialization, so the matrix earns nothing.

### Bloom filters or reachability bitmaps for descendancy

Precompute commit-reachability structures to answer "does B contain latest_revision?" directly.

*Rejected because:* the equality check on tree entries is strictly simpler and equally correct given the chain invariant — Merkle entries already capture chain position. Reachability bitmaps remain useful for general history queries but are not required for this one.

### CAS on the latest entry

Treat latest as a contended write target, advance via the mutable store's `compare_and_swap` at push.

*Rejected because:* the exclusive lock already guarantees that only the holder can advance latest while it is held. No concurrent writer exists. CAS adds protocol complexity (snapshot field on lock acquisition, expected-value tracking on latest writes) to defend against a race that the lock already prevents. CAS is still useful during Phase 2 backfill, where it provides safe absence-conditional writes; it just isn't needed in the steady-state lock-and-push protocol.

## Prior Art

- **[Perforce](https://www.perforce.com/manuals/p4guide/Content/P4Guide/resolve.lock.exclusive.html).** Exclusive locking comes in two flavors: the `+l` filetype modifier (prevents others from opening for edit) and the `p4 lock` command (prevents others from submitting). Both are *per-branch / per-stream*: the same logical file in two different streams can be independently locked, edited, and submitted, and the divergence surfaces only at integration. Under Perforce's stream model (mainline, release, development, task, virtual stream types with parent-child hierarchy and copy-down / merge-up flow), this is structurally the same gap this LEP closes — Perforce shops that need cross-stream lock causality build custom server-side triggers that walk the stream hierarchy ([discussed at length on the perforce-user list](https://perforce-user.perforce.narkive.com/s3Mqig5m/p4-exclusive-checkout-across-branches)). Recent additions in Helix 2025.2 introduce a "global exclusive lock" for the DVCS workflow (taken on `p4 edit --remote=remote` and held until push or revert against the shared server), a partial answer scoped to personal-server topologies rather than general stream-graph causality. The lesson: as soon as branching enters the picture, lock primitives need branching-graph awareness to be sound; Perforce demonstrates this by negative example, lacking that awareness in the core protocol.
- **[Git LFS file locking](https://github.com/git-lfs/git-lfs/wiki/File-Locking).** Provides server-side locks for LFS-tracked files, held until released or pushed. Locks are **repository-wide and keyed by file path**, the opposite of Perforce's per-branch scoping: a lock taken in one branch prevents *other* users from editing that path on any branch ([explainer](https://www.vikram.codes/blog/2024/3/22/git-lfs-file-locking)). This avoids the divergent-edits-across-branches failure mode that Perforce streams have, but introduces three weaknesses this LEP avoids by construction:
  - **Path-keyed identity:** because the lock is keyed by file path, renaming a locked file loses the lock — "when you rename an exclusively-locked file, the lock is lost. You'll have to lock it again to keep it locked." This LEP keys on `file_id`, which is stable across renames.
  - **No causality at acquisition:** the cross-branch lock blocks *other users* on other branches, but does not check whether the *requesting branch* has observed the latest committed edit before granting. A stale branch can acquire and edit, and the divergence is detected at push or merge rather than at lock acquisition. This LEP makes the causality check primary.
  - **Cross-branch overreach with no scoping primitive:** because every lock is repository-wide, a release-branch hotfix on a binary asset blocks every other branch from editing that asset until it's released or merged — the [exact merge-friction problem](https://gitlab.com/gitlab-org/gitlab/-/issues/224462) Git LFS users report. GitLab's "two modes" (exclusive vs. default-branch) is a workaround; this LEP solves it with first-class scopes that branches join independent of the branch graph. The lesson: a *repository-wide path-keyed lock* over-blocks; a *per-branch lock* under-blocks; this LEP's *per-scope file_id-keyed chain* is the construction that does neither. Also worth flagging as a cautionary tale: [the GitHub web UI bypasses LFS locks entirely](https://dev.to/devactivity/unlocking-productivity-why-githubs-web-ui-must-respect-git-lfs-locks-2afb) — an enforcement gap that demonstrates the importance of routing every mutation path through the lock check rather than trusting clients to consult it.
- **[Plastic SCM / Unity Version Control smart locks](https://docs.unity.com/en-us/unity-version-control/smart-locks).** Server-side cross-branch awareness for locks, denying a lock when a newer version exists elsewhere. The closest known analog to this proposal; informed by the same game-asset workflows. Its **multiple destination branches** capability is the closest existing precedent for the scope concept here: each destination branch is an independent lock scope, locks on the same file may be held simultaneously across destinations without conflict, and check-ins on non-destination branches produce a `Retained` state requiring merge-to-destination — a different take on the cross-scope merge problem this LEP resolves with chain-advancing merges into the target scope. This proposal considers its design superior because it decouples causality scoping from branch hierarchies: the user acquiring a lock does not need to name or know about a destination branch — the scope is determined by the requesting branch's own scope membership (set once at branch creation), so lock-time UX needs only "lock this file," not "lock this file targeting that destination." The branch graph and the lock-causality partition are independent dimensions, where Plastic conflates them.

## Unresolved Questions

All nine questions this proposal left open were settled during specification. They stay listed as the record of what was open at approval, each with the answer it reached; the reasoning behind each lives in the implementation spec.

| Question | Settled as |
| --- | --- |
| **Multi-shard push atomicity mechanism.** 2PC, saga with compensation, or co-shard-by-commit forcing. | None of them — cross-key atomicity isn't needed. The concluding revision is recorded on the lock entry before the branch advances, so the chain write is a forward-only replay that three independent paths can complete. Co-sharding is rejected outright: it would recreate the hot partition the key derivation exists to avoid. |
| **Cherry-pick and revert of unmergeable-file edits.** Denied at apply time, or allowed as a chain advance requiring a lock? | Not a special case. Anything that changes an unmergeable file's content is a chain advance in the target scope on identical terms, so edit, merge, cherry-pick, revert, and cross-scope merge pass one gate rather than four. A revert landing on the content the chain already holds is adoption and needs nothing. |
| **`--supersede` authorization.** Who may roll a latest back? | Two repository-level capabilities, neither on the branch: branch ownership is the wrong gate because force-release exists precisely when the holder is unreachable, and write access to the requesting branch is wrong because the target is another branch's session. |
| **Branch scope reassignment.** Should a branch be movable between scopes? | Immutable in the first version, with the shape kept forward-compatible. The interesting precondition needs a reachability query Lore doesn't have cheaply, and the demand is speculative. |
| **Cross-scope merge ergonomics.** Per-file confirmation, a summary, or silent? | A pre-flight summary and one confirmation. Per-file confirmation is unusable for a backport touching hundreds of assets, and silence is wrong for the one operation that crosses a causality boundary deliberately. |
| **Scope merge / split.** Can scopes be combined or divided? | No verb, now or later. Both are already expressible through branch-level merges, and a scope-level merge would have to reconcile two independent tips of one file with no common chain, which is exactly the unresolvable conflict this proposal exists to prevent. Exposing a verb implying otherwise would be worse than not having one. |
| **Default scope semantics.** Which lifecycle verbs to expose, and is the name reserved? | Rename only, and the name is reserved. Deriving the default scope's identifier from the repository makes the rest fall out: no creation write, no migration, no lookup. |
| **Quiescent-latest collapse trigger.** Sweep, event-driven, or lazy? | Not built in the first version. Its consensus check as described scales with `branches × files`, which is what Goal 3 forbids, so building it as specified would trade a storage question for a latency one. A gauge ships instead, and the trigger is lazy-at-acquire if it's ever built. |
| **Enforcement policy mechanism.** Where does the policy live? | Two repository-metadata keys read per request. Not per-scope, because a scope is a causality partition rather than a security boundary and varying strictness within a repository would make a file's protection depend on which branch a user started from. Not server-level, because one operator switch silently relaxing every repository is the policy-drift risk in its worst form. |

Specification also opened questions this proposal didn't anticipate, and settled them: whether locks belong to the repository that owns a file or the one a caller reached it through, whether a branch sitting ahead of the chain should be granted or denied, whether a push holding no lock can advance a chain it doesn't diverge from, and whether the two enforcement axes can apply to the same set of clients. Each is answered in the sections above.
