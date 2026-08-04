//! Single-writer discipline for the goal plan file.
//!
//! `plan.md` is the goal's completion contract: the acceptance criteria, the
//! dual Exec/Audit checklist, the criterion dependency table. Every harness
//! update to it is a read-modify-write — read the body, rewrite one column,
//! write it back — and there are several of them (checklist insertion, audit
//! marks cleared per criterion, audit marks set on acceptance, the fallback
//! plan).
//!
//! Serially that is fine. With criteria implemented concurrently it is not:
//! two overlapping read-modify-writes silently drop one side's edit, and the
//! edit being dropped is an audit mark. Losing a cleared mark hands out audit
//! credit no skeptic gave — the exact failure the dual checklist exists to
//! prevent, arriving with no error and no failing test.
//!
//! So every harness write to a plan file goes through [`with_plan_lock`],
//! which serializes read-modify-write sequences per path. The lock is
//! in-process, which is sufficient here because every writer — parent session,
//! worker subagents, the strategist — runs as a thread in this process, not as
//! a separate command.
//!
//! What this does NOT cover: a model editing `plan.md` through its own write
//! tool. No lock can make that safe, because the model reads the file in one
//! turn and writes it in another. That is why plan authority is a prompt-level
//! contract (only the goal-level session may edit the plan) rather than
//! something enforced here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// Per-path locks, created on first use and kept for the process lifetime.
///
/// Keyed by path rather than one global lock so two concurrent goals (or a
/// test suite running goals in parallel) do not serialize against each other.
/// The map itself is only held long enough to clone an `Arc`, never across the
/// file I/O.
static PLAN_LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<()>>>>> = Mutex::new(None);

fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    let mut guard = PLAN_LOCKS.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    Arc::clone(map.entry(path.to_path_buf()).or_default())
}

/// Run `write` with exclusive access to `path`'s plan file.
///
/// `write` must contain the WHOLE read-modify-write sequence; wrapping only the
/// write half would serialize nothing useful. It is called synchronously while
/// the lock is held, so it should do file I/O and nothing else — no awaits, no
/// spawning, and no re-entry into another plan write on the same path (that
/// would deadlock).
pub(crate) fn with_plan_lock<T>(path: &Path, write: impl FnOnce() -> T) -> T {
    let lock = lock_for(path);
    let _guard = lock.lock();
    write()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this module exists to prevent, reproduced against the lock:
    /// N threads each clearing a different column of the same file must all
    /// land, because each one read-modify-writes under the lock.
    #[test]
    fn concurrent_read_modify_writes_do_not_lose_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        let rows: Vec<String> = (0..24).map(|i| format!("| [x] | row{i} |")).collect();
        std::fs::write(&path, rows.join("\n")).unwrap();

        std::thread::scope(|s| {
            for i in 0..24 {
                let path = path.clone();
                s.spawn(move || {
                    with_plan_lock(&path, || {
                        let body = std::fs::read_to_string(&path).unwrap();
                        let updated = body
                            .replace(&format!("| [x] | row{i} |"), &format!("| [ ] | row{i} |"));
                        std::fs::write(&path, updated).unwrap();
                    });
                });
            }
        });

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            !body.contains("[x]"),
            "every thread's edit must survive; a lost one is a granted audit \
             mark nobody earned:\n{body}"
        );
    }

    #[test]
    fn different_paths_do_not_serialize_against_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        // Holding `a` must not block `b`; if it did, this would deadlock.
        with_plan_lock(&a, || {
            with_plan_lock(&b, || {
                std::fs::write(&b, "b").unwrap();
            });
        });
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
    }

    #[test]
    fn the_same_path_returns_the_same_lock() {
        let a = Path::new("plan.md");
        assert!(
            Arc::ptr_eq(&lock_for(a), &lock_for(Path::new("plan.md"))),
            "a fresh lock per call would serialize nothing"
        );
    }
}
