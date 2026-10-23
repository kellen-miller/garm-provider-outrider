//! Administrator-controlled, persistent admission for new allocation dispatch.

use crate::config::AdmissionSettings;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::{
    ffi::CString,
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Component,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionWindow {
    version: u32,
    opened_at_unix: u64,
    expires_at_unix: u64,
}

/// Read current admission on every call. Missing configuration means normal,
/// unbounded operation; configured state fails closed on every read/validation error.
pub fn allocation_is_open(settings: Option<&AdmissionSettings>) -> bool {
    let Some(settings) = settings else {
        return true;
    };
    let Some(window) = read_window(settings) else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };

    window.is_open(now.as_secs(), settings)
}

impl AdmissionWindow {
    fn is_open(&self, now: u64, settings: &AdmissionSettings) -> bool {
        self.version == 1
            && self.opened_at_unix <= now
            && now < self.expires_at_unix
            && self
                .expires_at_unix
                .checked_sub(self.opened_at_unix)
                .is_some_and(|duration| duration > 0 && duration <= settings.max_window_seconds)
    }
}

#[cfg(unix)]
fn read_window(settings: &AdmissionSettings) -> Option<AdmissionWindow> {
    if !settings.state_file.is_absolute() || settings.max_window_seconds == 0 {
        return None;
    }

    // Each component is opened relative to its verified directory descriptor.
    // This rejects symlinks without a metadata/open replacement race.
    let mut file = File::open("/").ok()?;
    // SAFETY: geteuid has no pointer arguments or memory-safety preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    let components = settings.state_file.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            if index == 0 && *component == Component::RootDir {
                continue;
            }

            return None;
        };
        let name = CString::new(name.as_bytes()).ok()?;
        let directory = index + 1 < components.len();
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let flags = if directory {
            flags | libc::O_DIRECTORY
        } else {
            flags
        };
        // SAFETY: name is NUL terminated; file owns a live directory descriptor.
        let descriptor = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return None;
        }

        // SAFETY: openat returned a fresh descriptor; this File takes sole ownership.
        file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata().ok()?;
        if (metadata.uid() != 0 && metadata.uid() != effective_uid)
            || metadata.mode() & 0o022 != 0
            || (directory && !metadata.is_dir())
            || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
        {
            return None;
        }
    }

    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }

    serde_json::from_slice(&bytes).ok()
}

#[cfg(not(unix))]
fn read_window(_settings: &AdmissionSettings) -> Option<AdmissionWindow> {
    None
}
