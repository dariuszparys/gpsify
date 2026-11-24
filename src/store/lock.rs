//! Exclusive PID lock at `jobs/<id>/propose.lock` (shared by propose, apply, undo).

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::store::db::StoreError;

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return false;
    };
    if pid_i32 <= 0 {
        return false;
    }
    // Safety: signal 0 probes process existence without delivering a signal.
    let rc = unsafe { kill(pid_i32, 0) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    matches!(err.raw_os_error(), Some(code) if code == 1 /* EPERM */)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

fn read_pid(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

pub struct JobExclusiveLock {
    path: PathBuf,
    held: bool,
}

impl JobExclusiveLock {
    pub fn new(path: PathBuf) -> Self {
        Self { path, held: false }
    }

    pub fn acquire(&mut self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.path.is_file() {
            if let Some(existing) = read_pid(&self.path) {
                if pid_alive(existing) {
                    return Err(StoreError::Conflict(format!(
                        "another propose/apply holds the job lock (pid {existing})"
                    )));
                }
            }
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(StoreError::Io(err)),
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|err| {
                if err.kind() == ErrorKind::AlreadyExists {
                    StoreError::Conflict("another propose/apply holds the job lock".to_string())
                } else {
                    StoreError::Io(err)
                }
            })?;
        write!(file, "{}", std::process::id())?;
        self.held = true;
        Ok(())
    }

    pub fn release(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        if read_pid(&self.path) == Some(std::process::id()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for JobExclusiveLock {
    fn drop(&mut self) {
        self.release();
    }
}
