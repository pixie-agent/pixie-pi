//! Per-file mutation lock (`core/tools/file-mutation-queue.ts`).
//!
//! Concurrent write/edit calls to the *same* file must be serialized; this
//! hands out one async mutex per canonical path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;

type PathLocks = std::sync::Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>;

static LOCKS: OnceLock<PathLocks> = OnceLock::new();

fn locks() -> &'static PathLocks {
    LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Acquire the per-file mutex, canonicalizing the path. Falls back to the
/// literal path if canonicalization fails (e.g. file does not yet exist).
async fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut map = locks().lock().unwrap();
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Run `f` while holding the per-file lock for `path`.
pub async fn with_file_lock<F, R>(path: &Path, f: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let lock = lock_for(path).await;
    // Actually acquire the per-file mutex: `lock_for` only *hands out* the
    // shared mutex for this path; without this `.lock().await` two concurrent
    // edits to the same file would race (lost update).
    let _guard = lock.lock().await;
    f.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn serializes_concurrent_access_to_the_same_path() {
        // Many tasks run critical sections on the same path; they must not
        // overlap. Before the fix `with_file_lock` never acquired the mutex, so
        // every task entered its section at once and `max_in_flight` was > 1.
        let path: PathBuf = std::env::temp_dir().join("pi-file-mutex-test-fixed.bin");
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            handles.push(tokio::spawn(async move {
                with_file_lock(&path, async {
                    let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_in_flight.fetch_max(n, Ordering::SeqCst);
                    // Yield points so concurrent (buggy) runs would interleave.
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }

        assert_eq!(
            max_in_flight.load(Ordering::SeqCst),
            1,
            "concurrent same-file critical sections overlapped"
        );
    }
}
