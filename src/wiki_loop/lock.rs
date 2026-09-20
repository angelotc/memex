//! Process singleton locking per role, following the house `IngestLease` pattern
//! (`src/lease.rs`): `std::fs::File::try_lock` + a JSON holder line for diagnostics.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct LockHolder {
    pid: u32,
    role: String,
    started_at: String,
}

pub struct RoleLock {
    _file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl RoleLock {
    /// Try to acquire the lock for `role` without blocking. Fails fast with a holder
    /// description when another instance is running — cron overlap exits cleanly.
    pub fn try_acquire(lock_dir: &Path, role: &str) -> Result<Self> {
        std::fs::create_dir_all(lock_dir)
            .with_context(|| format!("creating lock dir {}", lock_dir.display()))?;
        let path = lock_dir.join(format!("{role}.lock"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)
            .with_context(|| format!("opening lock file {}", path.display()))?;

        use std::io::Read;
        match file.try_lock() {
            Ok(()) => {
                let _ = file.set_len(0);
                let holder = LockHolder {
                    pid: std::process::id(),
                    role: role.to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                };
                let line = serde_json::to_string(&holder).unwrap_or_default();
                std::io::Write::write_all(&mut file, line.as_bytes()).ok();
                Ok(Self { _file: file, path })
            }
            Err(_) => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                anyhow::bail!(
                    "another wiki-loop '{}' instance holds {} ({})",
                    role,
                    path.display(),
                    holder.trim()
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_then_releases() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("locks");
        let _first = RoleLock::try_acquire(&dir, "maintainer").expect("first lock");
        assert!(RoleLock::try_acquire(&dir, "maintainer").is_err());
        drop(_first);
        let _second = RoleLock::try_acquire(&dir, "maintainer").expect("lock after release");
    }
}
