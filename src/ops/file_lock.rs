//! Advisory exclusive file locking via `flock` (WHI-524 / WHI-1407).
//!
//! Shared by [`crate::execution::breaker::SecureStore`] and
//! [`crate::notify::state::StateHandle`] — both need a single-flight exclusive lock
//! on a small state file and both used to carry their own byte-identical copy of
//! this trait+impl before this extraction.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Advisory exclusive lock helper using `flock`. `try_lock_exclusive` never blocks
/// (`LOCK_NB`): a lock already held by another process (or another handle in this
/// one) is reported as an error, not waited on.
pub trait FileExtLock {
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

/// `true` when `error` is the "would block" shape `flock(..., LOCK_NB)` returns for
/// an already-held lock. Checks both `ErrorKind::WouldBlock` (the common mapping) and
/// the raw `EWOULDBLOCK`/`EAGAIN` errno directly, since not every platform maps flock
/// contention to `ErrorKind::WouldBlock` consistently.
pub fn is_lock_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        || error.raw_os_error() == Some(libc::EAGAIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("amms-file-lock-{name}-{nanos}"))
    }

    #[test]
    fn a_second_handle_on_the_same_file_cannot_also_lock_it() {
        let path = tmp_path("contend");
        let a = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        a.try_lock_exclusive().unwrap();

        let b = OpenOptions::new().write(true).open(&path).unwrap();
        let err = b.try_lock_exclusive().unwrap_err();
        assert!(is_lock_contended(&err));

        drop(a);
        // Released on close/drop -> a fresh handle can now lock it.
        b.try_lock_exclusive().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
