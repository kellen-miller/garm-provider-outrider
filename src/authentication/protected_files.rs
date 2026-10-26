//! Descriptor-relative access to protected credential inputs and managed outputs.

use super::AuthError;
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

const SECRET_LIMIT: u64 = 64 * 1024;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) struct TokenFile {
    directory: File,
    name: CString,
}

pub(super) fn read_secret(path: &Path) -> Result<Vec<u8>, AuthError> {
    let output = TokenFile::open(path)?;
    let file = output.open_existing()?.ok_or(AuthError::PrivateFile)?;
    let mut bytes = Vec::new();
    file.take(SECRET_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::PrivateFile)?;
    if bytes.len() as u64 > SECRET_LIMIT {
        return Err(AuthError::PrivateFile);
    }

    Ok(bytes)
}

impl TokenFile {
    pub(super) fn open(path: &Path) -> Result<Self, AuthError> {
        if !path.is_absolute()
            || path.as_os_str().as_bytes().last() == Some(&b'/')
            || path
                .as_os_str()
                .as_bytes()
                .split(|byte| *byte == b'/')
                .any(|part| part == b"." || part == b"..")
        {
            return Err(AuthError::PrivateFile);
        }

        let components = path.components().collect::<Vec<_>>();
        let mut directory = File::open("/").map_err(|_| AuthError::PrivateFile)?;
        secure_directory(&directory)?;
        for component in components
            .iter()
            .skip(1)
            .take(components.len().saturating_sub(2))
        {
            let Component::Normal(name) = component else {
                return Err(AuthError::PrivateFile);
            };
            let name = CString::new(name.as_bytes()).map_err(|_| AuthError::PrivateFile)?;
            directory = open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            secure_directory(&directory)?;
        }

        let Some(Component::Normal(name)) = components.last() else {
            return Err(AuthError::PrivateFile);
        };
        let output = Self {
            directory,
            name: CString::new(name.as_bytes()).map_err(|_| AuthError::PrivateFile)?,
        };
        output.open_existing()?;
        Ok(output)
    }

    pub(super) fn same_directory(&self, other: &Self) -> Result<bool, AuthError> {
        let left = self
            .directory
            .metadata()
            .map_err(|_| AuthError::PrivateFile)?;
        let right = other
            .directory
            .metadata()
            .map_err(|_| AuthError::PrivateFile)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }

    pub(super) fn lock_directory(&self) -> Result<File, AuthError> {
        // Nonblocking lock; its descriptor remains owned by TokenRenewal through HTTP.
        // Open a fresh description: cloned descriptors share flock ownership.
        let dot = CString::new(".").map_err(|_| AuthError::PrivateFile)?;
        let lock = open_at(&self.directory, &dot, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        lock.try_lock().map_err(|_| AuthError::Locked)?;
        Ok(lock)
    }

    fn open_existing(&self) -> Result<Option<File>, AuthError> {
        // SAFETY: directory and name remain valid for the syscall. NOFOLLOW refuses
        // symlinks; NONBLOCK avoids hanging on a malicious FIFO before fstat.
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(AuthError::PrivateFile)
            };
        }

        // SAFETY: openat returned a fresh descriptor, exclusively owned here.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata().map_err(|_| AuthError::PrivateFile)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || !trusted_owner(metadata.uid())
        {
            return Err(AuthError::PrivateFile);
        }

        Ok(Some(file))
    }

    pub(super) fn publish(&self, content: &[u8]) -> Result<(), AuthError> {
        secure_directory(&self.directory)?;
        self.open_existing()?;
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = CString::new(format!(".outrider-token-{}-{sequence}", std::process::id()))
            .map_err(|_| AuthError::PrivateFile)?;
        let mut staged = open_at(
            &self.directory,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let result = (|| {
            staged
                .write_all(content)
                .map_err(|_| AuthError::PrivateFile)?;
            staged.sync_all().map_err(|_| AuthError::PrivateFile)?;
            // Recheck immediately before replacing; protected ancestors exclude
            // untrusted writers, and trusted owners already control this process.
            self.open_existing()?;
            // SAFETY: both names are NUL terminated and directory owns a live fd.
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } != 0
            {
                return Err(AuthError::PrivateFile);
            }

            self.directory
                .sync_all()
                .map_err(|_| AuthError::PrivateFile)
        })();
        if result.is_err() {
            // SAFETY: cleanup is confined to the staging name in our directory.
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), temporary.as_ptr(), 0) };
        }

        result
    }

    pub(super) fn invalidate(&self) -> Result<(), AuthError> {
        secure_directory(&self.directory)?;
        if self.open_existing()?.is_none() {
            return Ok(());
        }

        // SAFETY: the protected directory and validated relative name are live.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) } != 0 {
            return Err(AuthError::PrivateFile);
        }

        self.directory
            .sync_all()
            .map_err(|_| AuthError::PrivateFile)
    }
}

fn secure_directory(directory: &File) -> Result<(), AuthError> {
    let metadata = directory.metadata().map_err(|_| AuthError::PrivateFile)?;
    if !metadata.is_dir() || metadata.mode() & 0o022 != 0 || !trusted_owner(metadata.uid()) {
        return Err(AuthError::PrivateFile);
    }

    Ok(())
}

fn trusted_owner(uid: u32) -> bool {
    // SAFETY: geteuid has no memory-safety preconditions.
    uid == 0 || uid == unsafe { libc::geteuid() }
}

fn open_at(
    directory: &File,
    name: &CString,
    flags: i32,
    mode: libc::c_uint,
) -> Result<File, AuthError> {
    // SAFETY: name is NUL terminated; directory owns a live fd. Mode is supplied
    // for O_CREAT; the extra vararg is ignored for opens without that flag.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(AuthError::PrivateFile);
    }

    // SAFETY: openat returned a fresh fd; File takes sole ownership.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
