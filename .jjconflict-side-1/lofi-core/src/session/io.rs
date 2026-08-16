//! One background transcript-IO worker per session store.
//!
//! Every read-heavy transcript walk — the /resume picker scan, the /tree
//! snapshot, and tree-viewport hydration — used to run on a freshly spawned
//! thread per action. Each thread got its own glibc arena whose freed pages
//! stayed mapped at the high-water mark, so RSS ratcheted up on every picker
//! action. Funneling the work through a single long-lived worker per store
//! reuses one arena across all of them, so the peak is paid once.
//!
//! The worker additionally owns a registry of in-flight /tree snapshots.
//! Tree picking allocates a multi-MB `Vec<EventIndex>` plus parent/child
//! `HashMaps` on whichever thread builds them; if those allocations are freed
//! on a different thread (e.g. the UI dropping `Arc<Vec<EventIndex>>` after
//! the picker closes), glibc returns the pages to the *allocating* thread's
//! arena where they sit idle until that thread allocates again. Pinning the
//! snapshot's whole lifecycle to this worker keeps both the allocation and
//! the free on the same thread, so the freed slack is reusable on the next
//! /tree open instead of stuck in another thread's arena.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};

use super::store::SessionTreeSnapshot;

type Job = Box<dyn FnOnce(&mut WorkerState) + Send + 'static>;

/// Per-worker scratch state passed to every job. Today this holds the /tree
/// snapshot registry; new fields belong here when a future IO job needs
/// cross-call state on the worker.
#[derive(Default)]
pub struct WorkerState {
    tree_snapshots: HashMap<u64, SessionTreeSnapshot>,
    next_tree_snapshot_id: u64,
}

impl WorkerState {
    /// Store a snapshot, returning its handle. The handle is scoped to this
    /// worker — the caller is expected to round-trip every operation through
    /// the same worker (which the sink guarantees by keying on store root).
    pub fn insert_tree_snapshot(&mut self, snapshot: SessionTreeSnapshot) -> u64 {
        self.next_tree_snapshot_id += 1;
        let id = self.next_tree_snapshot_id;
        self.tree_snapshots.insert(id, snapshot);
        id
    }

    #[must_use]
    pub fn tree_snapshot(&self, id: u64) -> Option<&SessionTreeSnapshot> {
        self.tree_snapshots.get(&id)
    }

    pub fn remove_tree_snapshot(&mut self, id: u64) {
        self.tree_snapshots.remove(&id);
        // The freed Vec<EventIndex> stays in this thread's glibc arena
        // otherwise. Trim so the next /tree open doesn't ratchet RSS.
        crate::malloc_trim::release_freed_memory();
    }
}

fn workers() -> &'static Mutex<HashMap<PathBuf, SyncSender<Job>>> {
    static WORKERS: OnceLock<Mutex<HashMap<PathBuf, SyncSender<Job>>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Submit a closure to the IO worker for `root`, spawning the worker on first
/// use. Jobs run one at a time in submission order; a full channel drops the
/// job rather than blocking the caller.
///
/// The closure receives the worker's mutable state so long-lived jobs (tree
/// snapshot open / hydrate / close) can pin state on the worker thread.
pub fn submit(root: PathBuf, job: Job) {
    let tx = {
        let mut map = workers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = map.entry(root).or_insert_with(|| {
            let (tx, rx): (SyncSender<Job>, Receiver<Job>) = mpsc::sync_channel(8);
            std::thread::spawn(move || {
                let mut state = WorkerState::default();
                while let Ok(job) = rx.recv() {
                    job(&mut state);
                }
            });
            tx
        });
        tx.clone()
    };
    let _ = tx.try_send(job);
}
