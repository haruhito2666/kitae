//! Dev-mode helpers for `kitae dev` and generated sites: change detection
//! by mtime polling — no watcher deps, exact enough for a dev loop.

use std::path::Path;
use std::time::SystemTime;

/// One file's identity: path + mtime. Missing files report the epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    pub path: std::path::PathBuf,
    pub mtime: SystemTime,
}

fn mtime_of(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Snapshot every file under `dir` (recursive; files only). Empty when the
/// directory doesn't exist — watching an absent dir is legal and simply
/// never reports changes until it exists.
pub fn dir_stamps(dir: &Path) -> Vec<Stamp> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(dir_stamps(&p));
        } else {
            let mtime = mtime_of(&p);
            out.push(Stamp { path: p, mtime });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Re-scan `dir` and report whether anything changed since `stamps`
/// (added, removed, or touched). `stamps` is replaced with the new scan.
pub fn dir_changed(stamps: &mut Vec<Stamp>, dir: &Path) -> bool {
    let fresh = dir_stamps(dir);
    let changed = fresh != *stamps;
    if changed {
        *stamps = fresh;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_detect_creation_and_edits() {
        let dir = std::env::temp_dir().join(format!("kitae_dev_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut stamps = dir_stamps(&dir);
        assert!(stamps.is_empty());

        // Creation is a change.
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        assert!(dir_changed(&mut stamps, &dir));
        assert_eq!(stamps.len(), 1);
        // No change without a write.
        assert!(!dir_changed(&mut stamps, &dir));
        // Rewriting the file with new content bumps the mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        assert!(dir_changed(&mut stamps, &dir));
        // Removal is a change.
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        assert!(dir_changed(&mut stamps, &dir));
        assert!(stamps.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
