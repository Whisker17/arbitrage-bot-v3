//! Secure fd-relative store with exclusive lock (WHI-524).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("store already locked by another process")]
    Locked,
    #[error("insecure path or permissions: {0}")]
    Insecure(String),
    #[error("corrupt store: {0}")]
    Corrupt(String),
}

/// Namespaced durable directory for one `(chain, executor, signer)` scope.
pub struct SecureStore {
    dir: PathBuf,
    _lock: File,
    dir_fd: i32,
}

impl SecureStore {
    pub fn open(root: &Path, scope_name: &str) -> Result<Self, StoreError> {
        fs::create_dir_all(root)?;
        harden_dir(root)?;
        let dir = root.join(scope_name);
        if !dir.exists() {
            fs::create_dir(&dir)?;
        }
        harden_dir(&dir)?;
        refuse_symlink(&dir)?;

        let lock_path = dir.join("LOCK");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&lock_path)?;
        harden_file_meta(&lock_path)?;
        if let Err(e) = lock.try_lock_exclusive() {
            if e.kind() == io::ErrorKind::WouldBlock {
                return Err(StoreError::Locked);
            }
            // Some platforms map flock failure differently.
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) || e.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(StoreError::Locked);
            }
            return Err(StoreError::Io(e));
        }

        let dir_file = OpenOptions::new().read(true).open(&dir)?;
        let dir_fd = dir_file.into_raw_fd();
        Ok(Self {
            dir,
            _lock: lock,
            dir_fd,
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn wal_path(&self) -> PathBuf {
        self.dir.join("wal.v1")
    }

    pub fn checkpoint_path(&self) -> PathBuf {
        self.dir.join("checkpoint.v1")
    }

    pub fn pause_projection_path(&self) -> PathBuf {
        self.dir.join("pause.proj")
    }

    pub fn audit_projection_path(&self) -> PathBuf {
        self.dir.join("audit.proj")
    }

    /// Atomically write bytes to `name` via O_EXCL temp + fsync + rename + dir fsync.
    pub fn atomic_write(&self, name: &str, bytes: &[u8]) -> Result<(), StoreError> {
        refuse_symlink(&self.dir.join(name))?;
        let tmp_name = format!(".{name}.{}.tmp", std::process::id());
        let tmp_path = self.dir.join(&tmp_name);
        if tmp_path.exists() {
            let _ = fs::remove_file(&tmp_path);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, self.dir.join(name))?;
        // Directory fsync via the held dir fd.
        let dir = unsafe { File::from_raw_fd(self.dir_fd) };
        let sync_result = dir.sync_all();
        // Avoid closing the fd on drop — reclaim ownership.
        let _ = dir.into_raw_fd();
        sync_result?;
        harden_file_meta(&self.dir.join(name))?;
        Ok(())
    }

    pub fn append_wal(&self, frame: &[u8]) -> Result<(), StoreError> {
        let path = self.wal_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        harden_file_meta(&path)?;
        file.write_all(frame)?;
        file.sync_all()?;
        let dir = unsafe { File::from_raw_fd(self.dir_fd) };
        let sync_result = dir.sync_all();
        let _ = dir.into_raw_fd();
        sync_result?;
        Ok(())
    }

    pub fn read_file(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.dir.join(name);
        if !path.exists() {
            return Ok(None);
        }
        refuse_symlink(&path)?;
        harden_file_meta(&path)?;
        let mut file = File::open(&path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(Some(buf))
    }

    pub fn read_wal(&self) -> Result<Vec<u8>, StoreError> {
        Ok(self.read_file("wal.v1")?.unwrap_or_default())
    }

    pub fn truncate_wal(&self) -> Result<(), StoreError> {
        self.atomic_write("wal.v1", &[])
    }
}

impl Drop for SecureStore {
    fn drop(&mut self) {
        // Close directory fd.
        unsafe {
            let _ = File::from_raw_fd(self.dir_fd);
        }
    }
}

fn harden_dir(path: &Path) -> Result<(), StoreError> {
    refuse_symlink(path)?;
    let meta = fs::metadata(path)?;
    if !meta.is_dir() {
        return Err(StoreError::Insecure(format!("{path:?} is not a directory")));
    }
    let mut perms = meta.permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn harden_file_meta(path: &Path) -> Result<(), StoreError> {
    if !path.exists() {
        return Ok(());
    }
    refuse_symlink(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(StoreError::Insecure(format!("{path:?} is not a regular file")));
    }
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn refuse_symlink(path: &Path) -> Result<(), StoreError> {
    if path.exists() {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(StoreError::Insecure(format!("{path:?} is a symlink")));
        }
    }
    Ok(())
}

/// Advisory exclusive lock helper using `flock`.
trait FileExtLock {
    fn try_lock_exclusive(&self) -> io::Result<()>;
}

impl FileExtLock for File {
    fn try_lock_exclusive(&self) -> io::Result<()> {
        let rc = unsafe { libc::flock(self.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("amms-breaker-store-{nanos}"))
    }

    #[test]
    fn exclusive_lock_rejects_second_open() {
        let root = tmp_root();
        let a = SecureStore::open(&root, "scope-a").unwrap();
        let err = SecureStore::open(&root, "scope-a").err().expect("second open");
        assert!(matches!(err, StoreError::Locked), "{err}");
        drop(a);
        let _b = SecureStore::open(&root, "scope-a").unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_write_roundtrip() {
        let root = tmp_root();
        let store = SecureStore::open(&root, "scope-b").unwrap();
        store.atomic_write("pause.proj", b"paused=1").unwrap();
        assert_eq!(store.read_file("pause.proj").unwrap().unwrap(), b"paused=1");
        let _ = fs::remove_dir_all(root);
    }
}
