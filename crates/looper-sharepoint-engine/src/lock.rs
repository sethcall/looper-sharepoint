//! One host per data dir: an exclusive OS file lock on `<data_dir>/host.lock`.
//!
//! The OS releases the lock when the process exits, however it exits, so a crash never leaves a stale lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another looper-sharepoint host is already running for {dir} (pid {pid})")]
    Held { dir: PathBuf, pid: String },
    #[error("locking {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

/// Held for as long as this process is the host for a data dir.
#[derive(Debug)]
pub struct HostLock {
    _file: File,
    path: PathBuf,
}

impl HostLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        let path = data_dir.join("host.lock");
        let io = |source| LockError::Io { path: path.clone(), source };
        std::fs::create_dir_all(data_dir).map_err(io)?;
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut pid = String::new();
                let _ = file.read_to_string(&mut pid);
                return Err(LockError::Held { dir: data_dir.to_owned(), pid: pid.trim().to_string() });
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
        file.set_len(0).map_err(io)?;
        file.rewind().map_err(io)?;
        write!(file, "{}", std::process::id()).map_err(io)?;
        file.flush().map_err(io)?;
        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_until_first_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let a = HostLock::acquire(dir.path()).unwrap();
        let err = HostLock::acquire(dir.path()).unwrap_err();
        assert!(err.to_string().contains(&std::process::id().to_string()), "{err}");
        drop(a);
        HostLock::acquire(dir.path()).unwrap();
    }
}
