//! One background transcript-IO worker per session store.
//!
//! Every read-heavy transcript walk — the /resume picker scan, the /tree
//! snapshot, and tree-viewport hydration — used to run on a freshly spawned
//! thread per action. Each thread got its own glibc arena whose freed pages
//! stayed mapped at the high-water mark, so RSS ratcheted up on every picker
//! action. Funneling the work through a single long-lived worker per store
//! reuses one arena across all of them, so the peak is paid once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send + 'static>;

fn workers() -> &'static Mutex<HashMap<PathBuf, SyncSender<Job>>> {
    static WORKERS: OnceLock<Mutex<HashMap<PathBuf, SyncSender<Job>>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Submit a closure to the IO worker for `root`, spawning the worker on first
/// use. Jobs run one at a time in submission order; a full channel drops the
/// job rather than blocking the caller.
pub fn submit(root: PathBuf, job: Job) {
    let tx = {
        let mut map = workers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = map.entry(root).or_insert_with(|| {
            let (tx, rx): (SyncSender<Job>, Receiver<Job>) = mpsc::sync_channel(8);
            std::thread::spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            });
            tx
        });
        tx.clone()
    };
    let _ = tx.try_send(job);
}
