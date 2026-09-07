// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cross-process exclusion for one on-disk store directory, and the epoch that
//! says whether another process changed it while this one was not holding it.
//!
//! # What the flock means
//!
//! **It says which process is using the store.** It is the lock *between* processes,
//! and nothing else: concurrency between tasks inside one process is the store's own
//! mutexes — the per-bucket `RwLock`, `flush_lock`, `serialize_lock` — and never
//! this.
//!
//! So it is held for as long as this process is using the store, and a *use* is a
//! span, not a call: a `lore_storage_open` handle from open to close, or one
//! repository command. Simultaneous uses in one process are counted, and the flock
//! is taken by the first and released by the last. An operation inside a span joins
//! the claim already held; taking one per operation would say nothing about whether
//! anyone has the store, and would put a lock acquisition and an epoch read on every
//! call.
//!
//! A store that has been *closed* is not in use, even while the keep-alive cache
//! still holds its in-memory state. That state is retained memory with no claim
//! attached, which is exactly why the epoch below exists.
//!
//! One exception to the span rule: a store with unflushed modifications keeps the
//! flock past the end of the span, because a process must not sit on changes another
//! process cannot see.
//!
//! **A process with nothing open blocks nobody.** That is what makes a keep-alive
//! worth having: the state stays, the claim does not.
//!
//! **No store flock is ever held while a repository lock is being acquired**, so the
//! two cannot form a cycle. That matters because `FSLock`'s wait is unbounded: a
//! cycle is not a slow path, it is two processes that never return. The containment
//! is what makes a long-held claim safe — without it, holding a store across a long
//! run is exactly how the cycle formed.
//!
//! The exclusion lives *here* rather than in `lore_revision`, so both ways into a
//! store — a repository context and a storage handle — get it, and neither can go
//! around it.
//!
//! # Why an epoch, and when it is checked
//!
//! Keeping the in-memory state after the claim is released is the whole point — it
//! is what lets a closed store be reopened without re-reading it — and it is only
//! sound if the state is proven unchanged. **A store must never serve in-memory
//! state that another process modified on disk while this one held no claim.**
//!
//! So the check belongs to the moment the store goes back into use, not to each
//! operation: that transition is the only point at which another process could have
//! written since this one last looked.
//!
//! The epoch is a counter in the store directory, read and written only under the
//! flock. A writer advances it **before** its first write, so a writer that dies
//! mid-write has already told every other process that its state is worthless —
//! advancing afterwards would leave a half-written store looking untouched.
//! Readers do not advance it, so two readers never invalidate each other.
//!
//! [`StoreGuard::is_stale`] is the answer, and it fails safe: an epoch that is
//! absent, unreadable, or different from the one this store last observed all
//! report stale. Only an exact match of two values that were both actually read
//! reports fresh.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::fs::lock::FSLock;
use parking_lot::Mutex;

/// File carrying the store's epoch, inside the store directory.
const EPOCH_FILE: &str = "epoch";

/// What a caller intends to do while it holds the store lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Reads only.
    ///
    /// Does not advance the epoch, so concurrent readers in different processes
    /// never invalidate each other's in-memory state.
    Read,
    /// May modify the store on disk.
    ///
    /// Advances the epoch before the caller writes anything. That ordering is the
    /// point: it costs one small write, and it means a process that is killed
    /// mid-write cannot leave another process reusing state that predates it.
    Write,
}

/// Everything about the lock that has to move together.
///
/// Behind a `parking_lot::Mutex` rather than an async one because it is held for
/// a few field updates and never across an `.await` — the flock acquisition
/// itself happens outside it, serialised by [`StoreLock::gate`].
struct LockState {
    /// The flock, while it is held. `None` means this process holds nothing.
    held: Option<FSLock>,
    /// Operations in flight, across every store object sharing this directory.
    active: usize,
    /// Whether some store on this directory has modifications not yet on disk.
    ///
    /// Keeps the flock past the last operation: a process must not hold
    /// modifications that another process has no way to see.
    dirty: bool,
    /// Whether the epoch was already advanced during the current hold.
    ///
    /// One advance per hold is enough — the point is to mark the store as touched
    /// by this process, not to count writes — and it keeps a burst of writes from
    /// paying a file write each.
    advanced: bool,
    /// The lock file, kept open while the lock is *not* held.
    ///
    /// Re-acquiring is then one lock call rather than an open, a lock, an unlock and
    /// a close — and this lock is taken and released per operation, so that pair was
    /// being paid on every cold acquisition. `None` while the lock is held (the
    /// [`FSLock`] owns the file then) and before the first acquisition.
    ///
    /// Holding the descriptor means holding *that* file: if the lock file were
    /// deleted and recreated underneath, this process would keep locking the old
    /// inode while another locked the new one, and the two would not exclude. Nothing
    /// deletes it — it is created on demand and left — and deleting a store directory
    /// out from under an open store is already unsupported.
    spare: Option<std::fs::File>,
    /// The epoch on disk, as this process last established it.
    ///
    /// Authoritative while the flock is held, because nothing outside this process
    /// can write it then — which is what lets a joining operation decide staleness
    /// without touching the filesystem.
    current: Epoch,
}

/// The flock and epoch state for one store *directory*, shared by every store
/// object in this process that is open on it.
///
/// # Why this is not per store object
///
/// `flock` excludes by open file description, not by process, so two descriptions
/// on one file exclude **each other inside a single process** — verified: the
/// second `flock(LOCK_EX|LOCK_NB)` returns `EWOULDBLOCK`, and
/// [`FSLock::acquire_directory_lock`] retries that without a timeout. Any code that
/// drops one store and opens another on the same directory while a guard is alive
/// would therefore hang against itself, forever. Sharing the flock makes the second
/// open join the first instead.
struct PathLock {
    /// The store directory holding the lock file and the epoch file. Canonical, so
    /// two spellings of one directory are one entry in the registry.
    path: PathBuf,
    /// The lock file inside it, resolved once.
    ///
    /// `FSLock::acquire_directory_lock` canonicalizes on every call, which is one
    /// `lstat` per path component for a path that cannot change — measured at 8.6 µs
    /// at depth 7 and growing with depth, on a path taken per operation.
    lock_path: PathBuf,
    /// The bookkeeping shared by every store on this directory.
    state: Mutex<LockState>,
    /// Serialises the transitions that touch the filesystem — taking the flock,
    /// reading the epoch, advancing it — and is held across a caller's reload, so
    /// only one task per process does any of them.
    gate: Arc<tokio::sync::Mutex<()>>,
}

/// Every directory this process holds a lock for, so that one directory has one
/// flock however many stores are open on it.
///
/// `Weak`, so the entry goes when the last store on that directory does.
static PATH_LOCKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<PathLock>>>> =
    std::sync::OnceLock::new();

fn path_locks() -> &'static Mutex<HashMap<PathBuf, std::sync::Weak<PathLock>>> {
    PATH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One store object's view of its directory's lock.
///
/// The flock, the in-flight count and the dirty flag are shared per directory
/// ([`PathLock`]); [`Self::observed`] is not, because it describes *this store's*
/// in-memory state. Two stores open on one directory therefore never block each
/// other, and neither can serve state the other invalidated.
pub struct StoreLock {
    /// The directory's flock and epoch, shared with every other store on it.
    shared: Arc<PathLock>,
    /// The epoch this store's in-memory state was built against.
    ///
    /// `None` until this store has established one, and `None` never matches, so a
    /// store that has observed nothing can never claim its state is current.
    observed: Mutex<Option<Epoch>>,
}

/// Whether state built against `observed` can still be used, given `current`.
///
/// Fails safe. A store that has established nothing is stale; so is one whose last
/// look could not be read, and so is any disagreement. Only two definite answers
/// that match report fresh.
const fn is_stale(observed: Option<Epoch>, current: Epoch) -> bool {
    match observed {
        Some(Epoch::At(mine)) => !matches!(current, Epoch::At(theirs) if mine == theirs),
        Some(Epoch::Absent) => !matches!(current, Epoch::Absent),
        Some(Epoch::Unreadable) | None => true,
    }
}

impl StoreLock {
    /// A lock for the store directory at `path`.
    ///
    /// The directory must exist: the path is canonicalized so that two spellings of
    /// one directory share one flock, and canonicalizing requires it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if `path` cannot be canonicalized.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Arc<Self>> {
        let key = path.as_ref().canonicalize()?;
        let shared = {
            let mut locks = path_locks().lock();
            match locks.get(&key).and_then(std::sync::Weak::upgrade) {
                Some(existing) => existing,
                None => {
                    // Dead entries are swept here rather than on drop, which would
                    // need the key on a path that must stay allocation-free.
                    locks.retain(|_, held| held.strong_count() > 0);
                    let created = Arc::new(PathLock {
                        lock_path: key.join("lock"),
                        path: key.clone(),
                        state: Mutex::new(LockState {
                            held: None,
                            active: 0,
                            dirty: false,
                            advanced: false,
                            spare: None,
                            current: Epoch::Absent,
                        }),
                        gate: Arc::new(tokio::sync::Mutex::new(())),
                    });
                    locks.insert(key, Arc::downgrade(&created));
                    created
                }
            }
        };
        Ok(Arc::new(Self {
            shared,
            observed: Mutex::new(None),
        }))
    }

    /// Takes the lock for one operation, reporting whether the in-memory state
    /// built against the last acquisition can still be trusted.
    ///
    /// Cheap when this process already holds the flock and this store is current: a
    /// counter increment, with no filesystem work. Everything else goes through the
    /// gate.
    ///
    /// **A guard reporting [`StoreGuard::is_stale`] holds the gate**, and every
    /// other acquisition on the directory waits, until the caller either calls
    /// [`StoreGuard::note_refreshed`] or drops it. That is what keeps an operation
    /// from running against a store whose state is being dropped underneath it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the flock cannot be taken, or if a writer cannot
    /// advance the epoch — which fails the acquisition rather than proceeding,
    /// because a write nobody else can detect is the one thing this must not
    /// allow.
    pub async fn acquire(self: &Arc<Self>, intent: Intent) -> std::io::Result<StoreGuard> {
        if let Some(guard) = self.try_join(intent) {
            return Ok(guard);
        }
        let gate = Arc::clone(&self.shared.gate).lock_owned().await;
        // Re-checked under the gate: another task may have taken the flock, advanced
        // the epoch, or finished a refresh while this one waited.
        if let Some(guard) = self.try_join(intent) {
            return Ok(guard);
        }

        let held_already = self.shared.state.lock().held.is_some();
        if held_already {
            // Held by this process, but this store either is stale or is the first
            // writer of the hold. Neither needs the flock taken again.
            //
            // **Staleness is read before the advance below, never after.** A store is
            // not made stale by its own write: it is judged against what the epoch was
            // when it last agreed with disk, and advancing is how it announces the
            // write it is about to make.
            let stale = {
                let state = self.shared.state.lock();
                is_stale(*self.observed.lock(), state.current)
            };
            let advance = {
                let state = self.shared.state.lock();
                intent == Intent::Write && !state.advanced
            };
            if advance {
                // The current epoch is authoritative while the flock is held, so this
                // needs no read of its own — nothing outside this process can have
                // written it since.
                let known = self.shared.state.lock().current;
                // Outside the state lock: this writes a file.
                let epoch = self.shared.write_epoch(next_epoch(known))?;
                let mut state = self.shared.state.lock();
                state.advanced = true;
                state.current = Epoch::At(epoch);
            }
            return Ok(self.enter(gate, stale));
        }

        let spare = self.shared.state.lock().spare.take();
        let held = match spare {
            Some(file) => FSLock::acquire_open_file(file, &self.shared.lock_path).await?,
            None => FSLock::acquire_exact_path(&self.shared.lock_path).await?,
        };
        let on_disk = read_epoch(&self.shared.path);
        // Against what was on disk when the flock was taken, before this store's own
        // advance below.
        let stale = is_stale(*self.observed.lock(), on_disk);
        let current = match intent {
            Intent::Write => Epoch::At(self.shared.write_epoch(next_epoch(on_disk))?),
            Intent::Read => on_disk,
        };
        {
            let mut state = self.shared.state.lock();
            state.held = Some(held);
            state.advanced = intent == Intent::Write;
            state.current = current;
        }
        Ok(self.enter(gate, stale))
    }

    /// Counts this operation in, deciding staleness against what is now on disk.
    ///
    /// A fresh acquisition adopts `current` as this store's observed epoch and lets
    /// the gate go. A stale one adopts nothing and **keeps the gate**, because the
    /// caller has not reloaded yet — see [`StoreGuard::note_refreshed`].
    fn enter(self: &Arc<Self>, gate: tokio::sync::OwnedMutexGuard<()>, stale: bool) -> StoreGuard {
        let mut state = self.shared.state.lock();
        state.active = state.active.saturating_add(1);
        if !stale {
            *self.observed.lock() = Some(state.current);
            return StoreGuard {
                lock: Arc::clone(self),
                stale: false,
                gate: None,
            };
        }
        StoreGuard {
            lock: Arc::clone(self),
            stale: true,
            gate: Some(gate),
        }
    }

    /// Joins a hold this process already has, when doing so needs no filesystem
    /// work and this store's state is current. `None` sends the caller to the gate.
    fn try_join(self: &Arc<Self>, intent: Intent) -> Option<StoreGuard> {
        let mut state = self.shared.state.lock();
        // Nothing held is nothing to join.
        state.held.as_ref()?;
        if intent == Intent::Write && !state.advanced {
            return None;
        }
        // **This is what keeps an operation off a store being reloaded**, as well as
        // catching another store object on this directory advancing the epoch during
        // this very hold. A reload in flight has not adopted the epoch yet — only
        // `note_refreshed` does that — so every other acquisition on this store reads
        // as stale, goes to the gate, and waits there behind the guard doing the
        // reloading. A store that *is* current joins with no gate and no waiting,
        // which is what keeps a nested hold from blocking behind an unrelated one.
        if is_stale(*self.observed.lock(), state.current) {
            return None;
        }
        state.active = state.active.saturating_add(1);
        Some(StoreGuard {
            lock: Arc::clone(self),
            stale: false,
            gate: None,
        })
    }

    /// Records that the store has modifications not yet written to disk.
    ///
    /// Keeps the flock past the last operation until [`Self::clear_dirty`], so no
    /// other process can read a store this one has changed and not yet flushed.
    pub fn mark_dirty(&self) {
        self.shared.state.lock().dirty = true;
    }

    /// Records that everything modified has reached disk, releasing the flock if
    /// nothing else is holding it.
    pub fn clear_dirty(&self) {
        let mut state = self.shared.state.lock();
        state.dirty = false;
        Self::release_if_idle(&mut state);
    }

    /// Whether this process is holding the flock right now. For tests and
    /// diagnostics; callers should hold a [`StoreGuard`] instead of asking.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.shared.state.lock().held.is_some()
    }

    /// Drops the flock once nothing is in flight and nothing is unflushed.
    fn release_if_idle(state: &mut LockState) {
        if state.active == 0 && !state.dirty {
            if let Some(held) = state.held.take() {
                // Unlocked but not closed, so the next acquisition is one lock call.
                state.spare = held.release_keeping_file();
            }
            state.advanced = false;
        }
    }
}

impl PathLock {
    /// Writes `epoch` to the store directory, returning it.
    ///
    /// Plain, un-atomic, and safe: the file is only ever read or written by a
    /// process holding the flock, so there is no concurrent reader to tear.
    fn write_epoch(&self, epoch: u64) -> std::io::Result<u64> {
        std::fs::write(self.path.join(EPOCH_FILE), epoch.to_le_bytes())?;
        Ok(epoch)
    }
}

/// The next epoch after `current`, which wraps rather than saturating.
///
/// Comparison is equality, never ordering, so wrapping is harmless and a store
/// directory that was deleted and recreated — its epoch back at zero — reads as
/// changed rather than as older.
const fn next_epoch(current: Epoch) -> u64 {
    match current {
        Epoch::At(epoch) => epoch.wrapping_add(1),
        Epoch::Absent | Epoch::Unreadable => 1,
    }
}

/// What the store directory says about its epoch.
///
/// Three states, not two, and the distinction is load-bearing. **Absent is a
/// definite answer**: a store nobody has written since epochs existed has no file,
/// two successive looks agree, and state built against it is reusable. **Unreadable
/// is not an answer at all** — a short or unopenable file could be anything — so it
/// never compares equal, not even to itself.
///
/// Folding the two together would make a store that never gets written stale on
/// every single acquisition, reloading all 256 groups each time and never reusing
/// anything, which is the whole benefit the epoch exists to enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Epoch {
    /// No epoch file. Definite, and reusable against another absence.
    Absent,
    /// A file that could not be read as one. Never reusable.
    Unreadable,
    /// The epoch a writer recorded.
    At(u64),
}

/// What the store directory records right now.
fn read_epoch(path: &Path) -> Epoch {
    let file = path.join(EPOCH_FILE);
    if !file.exists() {
        return Epoch::Absent;
    }
    let Ok(bytes) = std::fs::read(file) else {
        return Epoch::Unreadable;
    };
    match <[u8; 8]>::try_from(bytes.as_slice()) {
        Ok(eight) => Epoch::At(u64::from_le_bytes(eight)),
        Err(_) => Epoch::Unreadable,
    }
}

/// One operation's hold on a store.
///
/// Releasing is `Drop`, so an operation that returns early — or panics — cannot
/// leave the flock held. Deliberately not `Clone`: a hold is a resource with a
/// release, and copying one would let the count outlive what it was counting.
pub struct StoreGuard {
    /// The lock this guard is counted against.
    lock: Arc<StoreLock>,
    /// Whether another process changed the store since this one last looked.
    stale: bool,
    /// The directory's gate, held only by a stale guard.
    ///
    /// A stale guard is one whose caller is about to drop and rebuild the store's
    /// in-memory state. Holding the gate for that span is what stops another
    /// operation joining the hold and running against state being torn down.
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl StoreGuard {
    /// Whether the in-memory state this store built before the flock was last
    /// released can still be used.
    ///
    /// `true` means another process wrote, or that nothing can prove it did not.
    /// The caller must discard what it has and reload before serving anything, then
    /// say so with [`Self::note_refreshed`].
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale
    }

    /// Records that the caller has rebuilt this store's state from disk.
    ///
    /// **Only this adopts the epoch.** An acquisition that reports stale deliberately
    /// leaves the store's observed epoch alone, so a reload that fails — or a
    /// `hold()` future dropped part way through one — leaves the store still knowing
    /// it is behind. The next acquisition reports stale again and the reload is
    /// retried, where committing the epoch at acquisition time would have left the
    /// store claiming to be current over state that predates another process's write.
    ///
    /// Releases the gate, so operations refused while the reload ran may proceed.
    /// A no-op on a guard that was not stale.
    pub fn note_refreshed(&mut self) {
        if self.gate.take().is_none() {
            return;
        }
        let state = self.lock.shared.state.lock();
        *self.lock.observed.lock() = Some(state.current);
    }

    /// The lock this guard holds, for marking the store dirty while it is held.
    #[must_use]
    pub fn lock(&self) -> &Arc<StoreLock> {
        &self.lock
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        // A stale guard dropped without `note_refreshed` is a reload that failed or
        // was cancelled. Its gate goes with it, so the directory is never wedged, and
        // the observed epoch is left behind, so the next acquisition retries.
        let mut state = self.lock.shared.state.lock();
        state.active = state.active.saturating_sub(1);
        StoreLock::release_if_idle(&mut state);
    }
}

/// Every store lock a command holds, kept together so a repository lock can be
/// built on top of them.
///
/// # Why this type exists
///
/// The repository flock must be **fully contained** within the store locks: it
/// may never be acquired without them, and it may never outlive them. Both halves
/// of that are ordering, and ordering left to reviewers is ordering that comes
/// back — this codebase already had the inversion once, with a store flock held
/// by a `lore_storage_open` handle while the repository flock was taken by a
/// `lore_revision` command, and neither process ever returned.
///
/// So the invariant is a type rather than a rule. A repository lock holder owns
/// one of these, and there is no way to build one without it, which makes
/// "repository flock without store flocks" unrepresentable rather than
/// discouraged. The holder must declare its own flock **before** this field, so
/// that Rust's field drop order releases the repository flock first and the store
/// locks second — the reverse of acquisition, which is what containment means on
/// the way out.
///
/// A hold covering no on-disk stores is legitimate and empty: an in-memory store
/// has no flock to take, and a remote one has no local state to guard.
#[derive(Default)]
pub struct StoreHold {
    /// The guards, released together when this drops.
    guards: Vec<StoreGuard>,
}

impl StoreHold {
    /// A hold over `guards`, which are released when it drops.
    #[must_use]
    pub fn new(guards: Vec<StoreGuard>) -> Self {
        Self { guards }
    }

    /// Whether any of the stores reported that another process changed it.
    ///
    /// A command that sees `true` must let each store reload before it reads
    /// anything, which the stores do for themselves on their own guards; this is
    /// for callers that want to know a reload happened.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.guards.iter().any(StoreGuard::is_stale)
    }

    /// How many on-disk stores this hold covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.guards.len()
    }

    /// Whether this hold covers no on-disk store, which is the in-memory case.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reload that never happened is never adopted.** The epoch is taken only by
    /// [`StoreGuard::note_refreshed`], so a caller whose reload failed — or whose
    /// future was dropped part way through one — leaves the store still knowing it is
    /// behind, and the next acquisition retries.
    #[tokio::test]
    async fn a_staleness_left_unacknowledged_is_reported_again() {
        let dir = dir("unacked");
        let lock = store_lock(&dir.0);

        // A write, so the directory has an epoch at all: with no epoch file there is
        // nothing to observe, and every acquisition is correctly stale forever.
        let first = lock.acquire(Intent::Write).await.expect("acquires");
        assert!(first.is_stale(), "nothing observed yet");
        // Dropped without acknowledging, as a failed or cancelled reload would be.
        drop(first);

        let second = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(
            second.is_stale(),
            "the reload never happened, so the store still knows it is behind"
        );
        drop(second);

        let mut third = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(third.is_stale());
        third.note_refreshed();
        drop(third);

        let fourth = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(
            !fourth.is_stale(),
            "and once acknowledged, it stops being reported"
        );
    }

    /// **Two stores on one directory share the flock but not the epoch.**
    ///
    /// `flock` excludes by open file description, so a second description in the same
    /// process would block against the first — forever, since the wait is unbounded.
    /// Sharing the lock is what makes a second store on the same directory join
    /// rather than hang. Their in-memory state is still their own, so one writing
    /// must leave the other knowing it is behind.
    #[tokio::test]
    async fn two_stores_on_one_directory_share_the_flock_but_not_the_epoch() {
        let dir = dir("shared");
        let first = store_lock(&dir.0);
        let second = store_lock(&dir.0);

        // A write first, so the directory has an epoch for either store to observe.
        let mut opening = second.acquire(Intent::Write).await.expect("acquires");
        opening.note_refreshed();
        drop(opening);

        // Taken while the first store's guard is alive: this is the acquisition that
        // would deadlock against a second flock on the same directory.
        let mut held = first.acquire(Intent::Write).await.expect("acquires");
        held.note_refreshed();
        let joined = second
            .acquire(Intent::Read)
            .await
            .expect("does not block on itself");
        assert!(
            joined.is_stale(),
            "the other store advanced the epoch under this one"
        );
        drop(joined);
        drop(held);

        assert!(
            !first.is_held(),
            "and the flock goes when the last of them leaves"
        );
    }

    /// A lock for a directory known to exist, so a test that means to check the lock
    /// does not silently check path resolution instead.
    fn store_lock(path: &std::path::Path) -> Arc<StoreLock> {
        StoreLock::new(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    /// A store directory to lock, removed with the test.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir(name: &str) -> Dir {
        let path =
            std::env::temp_dir().join(format!("lore-store-lock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a fresh directory");
        Dir(path)
    }

    /// **The flock is held only while there is work, and released when there is
    /// not.** This is the property the deadlock turned on: an idle process that
    /// keeps a store alive must hold nothing.
    #[tokio::test]
    async fn the_flock_is_held_only_while_an_operation_is_in_flight() {
        let dir = dir("in-flight");
        let lock = store_lock(&dir.0);
        assert!(
            !lock.is_held(),
            "nothing is held before the first operation"
        );

        // Acknowledged: a guard reporting stale holds the directory's gate until its
        // caller has reloaded, so a second acquisition would wait behind it.
        let mut first = lock.acquire(Intent::Read).await.expect("acquires");
        first.note_refreshed();
        assert!(lock.is_held());
        let second = lock.acquire(Intent::Read).await.expect("joins the hold");
        assert!(lock.is_held());

        drop(second);
        assert!(
            lock.is_held(),
            "the last operation out releases, not the first"
        );
        drop(first);
        assert!(!lock.is_held(), "an idle store holds nothing");
    }

    /// **A store with unflushed modifications keeps the flock past its last
    /// operation**, because another process must not read a store this one has
    /// changed and not yet written.
    #[tokio::test]
    async fn dirty_state_keeps_the_flock_until_it_is_flushed() {
        let dir = dir("dirty");
        let lock = store_lock(&dir.0);

        let guard = lock.acquire(Intent::Write).await.expect("acquires");
        lock.mark_dirty();
        drop(guard);
        assert!(lock.is_held(), "unflushed modifications hold the flock");

        lock.clear_dirty();
        assert!(!lock.is_held(), "and a flush releases it");
    }

    /// **A writer advances the epoch, a reader does not.** Two readers must not
    /// invalidate each other, and a writer must be visible to everyone.
    #[tokio::test]
    async fn only_a_writer_advances_the_epoch() {
        let dir = dir("advance");
        let lock = store_lock(&dir.0);

        drop(lock.acquire(Intent::Read).await.expect("acquires"));
        assert_eq!(
            read_epoch(&dir.0),
            Epoch::Absent,
            "a reader writes no epoch"
        );

        drop(lock.acquire(Intent::Write).await.expect("acquires"));
        assert_eq!(read_epoch(&dir.0), Epoch::At(1), "a writer advances it");

        drop(lock.acquire(Intent::Read).await.expect("acquires"));
        assert_eq!(
            read_epoch(&dir.0),
            Epoch::At(1),
            "and a later reader leaves it alone"
        );
    }

    /// **The epoch advances before the write, not after**, so a process killed
    /// mid-write has already invalidated everyone else.
    #[tokio::test]
    async fn the_epoch_advances_before_the_caller_writes() {
        let dir = dir("before");
        let lock = store_lock(&dir.0);

        let guard = lock.acquire(Intent::Write).await.expect("acquires");
        assert_eq!(
            read_epoch(&dir.0),
            Epoch::At(1),
            "advanced while the guard is still held"
        );
        drop(guard);
    }

    /// **A writer joining a hold a reader started still advances the epoch.**
    /// The cheap path must not let a write go unannounced because a reader
    /// happened to take the flock first.
    #[tokio::test]
    async fn a_writer_joining_a_readers_hold_still_advances() {
        let dir = dir("join");
        let lock = store_lock(&dir.0);

        // Acknowledged, because a stale guard holds the directory's gate until its
        // caller has reloaded — which is what keeps an operation off a store being
        // rebuilt, and which every real caller does inside `hold`.
        let mut reader = lock.acquire(Intent::Read).await.expect("acquires");
        reader.note_refreshed();
        assert_eq!(read_epoch(&dir.0), Epoch::Absent);

        let writer = lock.acquire(Intent::Write).await.expect("joins");
        assert_eq!(
            read_epoch(&dir.0),
            Epoch::At(1),
            "the writer announced itself"
        );

        // A second writer in the same hold does not pay another file write.
        let again = lock.acquire(Intent::Write).await.expect("joins");
        assert_eq!(
            read_epoch(&dir.0),
            Epoch::At(1),
            "one advance per hold is enough"
        );
        drop((reader, writer, again));
    }

    /// **Nothing this process did to itself reads as staleness.** A store that
    /// reacquires after its own writes must keep its in-memory state, or the
    /// reuse this exists for never happens.
    #[tokio::test]
    async fn a_store_is_not_stale_against_its_own_writes() {
        let dir = dir("self");
        let lock = store_lock(&dir.0);

        // The first acquisition of a store that has observed nothing is stale, and
        // acknowledging it is what a caller does after loading from disk.
        let mut first = lock.acquire(Intent::Write).await.expect("acquires");
        first.note_refreshed();
        drop(first);

        let again = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(
            !again.is_stale(),
            "this process wrote it; it knows the epoch"
        );
        drop(again);

        let third = lock.acquire(Intent::Write).await.expect("acquires");
        assert!(!third.is_stale(), "nor does its own advance make it stale");
    }

    /// **Another process's write is stale, and so is anything unprovable.**
    /// The other process is simulated by writing the epoch file directly, which
    /// is exactly what it would do.
    #[tokio::test]
    async fn another_process_writing_makes_the_state_stale() {
        let dir = dir("other");
        let lock = store_lock(&dir.0);

        let mut opening = lock.acquire(Intent::Read).await.expect("acquires");
        opening.note_refreshed();
        drop(opening);

        std::fs::write(dir.0.join(EPOCH_FILE), 7u64.to_le_bytes()).expect("writes");
        let mut after = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(after.is_stale(), "the epoch moved under us");
        after.note_refreshed();
        drop(after);

        // Having observed 7, an unchanged store is fresh again.
        let unchanged = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(!unchanged.is_stale());
        drop(unchanged);

        // And every unprovable answer is stale: a missing file, and a short one.
        std::fs::remove_file(dir.0.join(EPOCH_FILE)).expect("removes");
        let mut absent = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(absent.is_stale(), "no epoch proves nothing");
        absent.note_refreshed();
        drop(absent);

        std::fs::write(dir.0.join(EPOCH_FILE), [1u8, 2, 3]).expect("writes");
        let short = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(short.is_stale(), "an unreadable epoch proves nothing");
    }

    /// **The first acquisition of all is stale**, because a store that has
    /// observed nothing cannot claim its state matches anything.
    #[tokio::test]
    async fn the_first_acquisition_is_stale() {
        let dir = dir("first");
        let lock = store_lock(&dir.0);
        let first = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(first.is_stale());
    }

    /// **A hold releases every store it covers, together.** The repository lock
    /// will own one of these, so the moment it stops holding them is the moment
    /// this drops — which is what keeps the repository flock inside the store
    /// locks on the way out as well as the way in.
    #[tokio::test]
    async fn a_hold_releases_every_store_it_covers() {
        let one = dir("hold-one");
        let two = dir("hold-two");
        let first = store_lock(&one.0);
        let second = store_lock(&two.0);

        let hold = StoreHold::new(vec![
            first.acquire(Intent::Read).await.expect("acquires"),
            second.acquire(Intent::Write).await.expect("acquires"),
        ]);
        assert_eq!(hold.len(), 2);
        assert!(first.is_held() && second.is_held());
        assert!(hold.is_stale(), "neither store has observed anything yet");

        drop(hold);
        assert!(!first.is_held(), "released with the hold");
        assert!(!second.is_held(), "and so is the other");
    }

    /// An in-memory store has no flock, so a hold over nothing is legitimate
    /// rather than a bug to guard against.
    #[test]
    fn a_hold_over_no_on_disk_store_is_empty_and_fresh() {
        let hold = StoreHold::default();
        assert!(hold.is_empty());
        assert!(!hold.is_stale(), "nothing on disk cannot have gone stale");
    }

    /// A recreated store directory reads as changed rather than as older, which
    /// is why the comparison is equality and never ordering.
    #[tokio::test]
    async fn a_reset_epoch_reads_as_changed() {
        let dir = dir("reset");
        let lock = store_lock(&dir.0);

        std::fs::write(dir.0.join(EPOCH_FILE), 9u64.to_le_bytes()).expect("writes");
        drop(lock.acquire(Intent::Read).await.expect("acquires"));

        std::fs::write(dir.0.join(EPOCH_FILE), 0u64.to_le_bytes()).expect("writes");
        let after = lock.acquire(Intent::Read).await.expect("acquires");
        assert!(after.is_stale(), "backwards is still different");
    }
}
