//! Phase 4: prune every object entry whose fresh hard-link count is one,
//! independent of any selection pattern used during discovery.
//!
//! `st_nlink` is filesystem-wide, so pruning is safe even when `PATTERN`
//! restricted which visible paths Phase 2 walked: an object entry that
//! still backs an unselected visible link is never mistaken for garbage.

use super::reconcile::{FailureKind, PathFailure};
use super::scan::ObjectIndex;
use std::fs;
use std::os::unix::fs::MetadataExt;

/// Re-stat each retained object entry immediately before deciding, and
/// unlink it only when the fresh link count is exactly one. The Phase 1
/// link count is never reused for this decision.
pub(crate) fn prune(index: &ObjectIndex) -> (usize, Vec<PathFailure>) {
    let mut pruned = 0;
    let mut failures = Vec::new();

    for record in index.records() {
        let metadata = match fs::metadata(&record.path) {
            Ok(metadata) => metadata,
            Err(e) => {
                failures.push(PathFailure {
                    path: record.path.clone(),
                    kind: FailureKind::PruneFailed,
                    message: format!("re-statting before prune: {e}"),
                });
                continue;
            }
        };
        if metadata.nlink() != 1 {
            continue;
        }
        if let Err(e) = fs::remove_file(&record.path) {
            failures.push(PathFailure {
                path: record.path.clone(),
                kind: FailureKind::PruneFailed,
                message: format!("unlinking unreferenced object entry: {e}"),
            });
        } else {
            pruned += 1;
        }
    }

    (pruned, failures)
}
