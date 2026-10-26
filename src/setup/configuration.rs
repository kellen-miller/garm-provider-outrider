//! Protected configuration edits preserve original ownership and reject concurrent changes.

use super::SetupError;
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_EDIT: AtomicU64 = AtomicU64::new(0);

pub(super) struct ConfigurationFile {
    directory: File,
    _locked_file: File,
    name: CString,
    original: Vec<u8>,
    metadata: std::fs::Metadata,
}

impl ConfigurationFile {
    pub(super) fn open(path: &Path) -> Result<Self, SetupError> {
        if !path.is_absolute() {
            return Err(SetupError::ProtectedConfiguration);
        }

        let components = path.components().collect::<Vec<_>>();
        let mut directory = File::open("/").map_err(|_| SetupError::ProtectedConfiguration)?;
        for component in components
            .iter()
            .skip(1)
            .take(components.len().saturating_sub(2))
        {
            let Component::Normal(name) = component else {
                return Err(SetupError::ProtectedConfiguration);
            };
            let name =
                CString::new(name.as_bytes()).map_err(|_| SetupError::ProtectedConfiguration)?;
            directory = open_at(&directory, &name, true)?;
            let metadata = directory
                .metadata()
                .map_err(|_| SetupError::ProtectedConfiguration)?;
            if !trusted_owner(metadata.uid()) || metadata.mode() & 0o022 != 0 {
                return Err(SetupError::ProtectedConfiguration);
            }
        }

        let name = path.file_name().ok_or(SetupError::ProtectedConfiguration)?;
        let name = CString::new(name.as_bytes()).map_err(|_| SetupError::ProtectedConfiguration)?;
        let mut file = open_at(&directory, &name, false)?;
        let metadata = file
            .metadata()
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || !trusted_owner(metadata.uid())
            || metadata.mode() & 0o137 != 0
        {
            return Err(SetupError::ProtectedConfiguration);
        }

        let mut original = Vec::new();
        Read::by_ref(&mut file)
            .take(1_048_577)
            .read_to_end(&mut original)
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        if original.len() > 1_048_576 {
            return Err(SetupError::ProtectedConfiguration);
        }

        file.try_lock()
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        Ok(Self {
            directory,
            _locked_file: file,
            name,
            original,
            metadata,
        })
    }

    pub(super) fn text(&self) -> Result<&str, SetupError> {
        std::str::from_utf8(&self.original).map_err(|_| SetupError::Configuration)
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.original
    }

    pub(super) fn publish(&self, bytes: &[u8]) -> Result<bool, SetupError> {
        if bytes == self.original {
            return Ok(false);
        }

        let mut current = open_at(&self.directory, &self.name, false)?;
        let metadata = current
            .metadata()
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        let mut contents = Vec::new();
        Read::by_ref(&mut current)
            .take(1_048_577)
            .read_to_end(&mut contents)
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        if metadata.dev() != self.metadata.dev()
            || metadata.ino() != self.metadata.ino()
            || contents != self.original
        {
            return Err(SetupError::ConcurrentEdit);
        }

        let temporary = CString::new(format!(
            ".outrider-setup-{}-{}",
            std::process::id(),
            NEXT_EDIT.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| SetupError::ProtectedConfiguration)?;
        // SAFETY: directory and temporary are live descriptor/NUL-terminated name.
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(SetupError::ProtectedConfiguration);
        }

        // SAFETY: descriptor is newly created and ownership transfers to File.
        let mut staged = unsafe { File::from_raw_fd(descriptor) };
        let mode = libc::mode_t::try_from(self.metadata.mode() & 0o777)
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        let result = (|| {
            staged
                .write_all(bytes)
                .map_err(|_| SetupError::ProtectedConfiguration)?;
            // SAFETY: staged owns the descriptor; original metadata is verified.
            if unsafe { libc::fchown(staged.as_raw_fd(), self.metadata.uid(), self.metadata.gid()) } != 0
                // SAFETY: mode restores the verified regular file's original permissions.
                || unsafe { libc::fchmod(staged.as_raw_fd(), mode) } != 0
            {
                return Err(SetupError::ProtectedConfiguration);
            }

            staged
                .sync_all()
                .map_err(|_| SetupError::ProtectedConfiguration)?;
            // SAFETY: both names are relative to the same retained directory descriptor.
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } != 0
            {
                return Err(SetupError::ProtectedConfiguration);
            }

            self.directory
                .sync_all()
                .map_err(|_| SetupError::ProtectedConfiguration)?;
            Ok(true)
        })();
        if result.is_err() {
            // SAFETY: removes only the unique temporary name created above.
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), temporary.as_ptr(), 0) };
        }

        result
    }
}

fn trusted_owner(uid: u32) -> bool {
    // SAFETY: geteuid has no memory-safety preconditions.
    uid == 0 || uid == unsafe { libc::geteuid() }
}

fn open_at(parent: &File, name: &CString, directory: bool) -> Result<File, SetupError> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent owns a valid descriptor, name is NUL terminated.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(SetupError::ProtectedConfiguration);
    }

    // SAFETY: openat returned a fresh descriptor now owned by File.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
