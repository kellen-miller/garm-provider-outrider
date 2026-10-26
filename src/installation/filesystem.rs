//! Protected directory traversal and rollback-scoped installation publication.

use super::{FILE_LIMIT, InstallationError, invalid};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

pub(super) struct CreatedEntry {
    parent: File,
    name: CString,
    directory: bool,
    device: libc::dev_t,
    inode: libc::ino_t,
}

pub(super) struct InstallationTransaction {
    pub(super) entries: Vec<CreatedEntry>,
    pub(super) adopted_directories: Vec<OriginalDirectory>,
    pub(super) committed: bool,
}

pub(super) struct OriginalDirectory {
    directory: File,
    uid: u32,
    gid: u32,
    mode: u32,
}

fn c_name(name: &std::ffi::OsStr) -> Result<CString, InstallationError> {
    CString::new(name.as_bytes()).map_err(|_| invalid("NUL in pathname"))
}

fn open_at(parent: &File, name: &CString, directory: bool) -> io::Result<File> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent is live and name is NUL terminated; no pointers escape.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: descriptor is newly opened and ownership transfers to File.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn secure_directory(directory: &File, target_uid: Option<u32>) -> Result<(), InstallationError> {
    let metadata = directory.metadata()?;
    // SAFETY: identity query has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.mode() & 0o022 != 0
        || (metadata.uid() != 0 && metadata.uid() != uid && Some(metadata.uid()) != target_uid)
    {
        return Err(invalid(
            "directory ancestors must be owned by root or the installer/target user and not group/world writable",
        ));
    }

    Ok(())
}

pub(super) fn open_directory(
    path: &Path,
    target_uid: Option<u32>,
) -> Result<File, InstallationError> {
    let mut directory = File::open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        directory = open_at(&directory, &c_name(name)?, true)?;
        secure_directory(&directory, target_uid)?;
    }

    Ok(directory)
}

pub(super) fn open_member(root: &File, path: &Path) -> Result<File, InstallationError> {
    let mut directory = root.try_clone()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid("invalid package member path"));
        };
        let member = open_at(&directory, &c_name(name)?, components.peek().is_some())?;
        if components.peek().is_none() {
            return Ok(member);
        }

        secure_directory(&member, None)?;
        directory = member;
    }

    Err(invalid("empty package member path"))
}

pub(super) fn check_destination(path: &Path, uid: u32) -> Result<(), InstallationError> {
    let mut directory = File::open("/")?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            continue;
        };
        let directory_member = components.peek().is_some();
        match open_at(&directory, &c_name(name)?, directory_member) {
            Ok(member) => {
                if directory_member {
                    secure_directory(&member, Some(uid))?;
                } else {
                    let metadata = member.metadata()?;
                    if metadata.is_dir() {
                        secure_directory(&member, Some(uid))?;
                    } else if !metadata.is_file()
                        || metadata.nlink() != 1
                        || metadata.mode() & 0o022 != 0
                        || (metadata.uid() != 0 && metadata.uid() != uid)
                    {
                        return Err(invalid(
                            "existing destination must be a protected regular single-link file",
                        ));
                    }
                }

                directory = member;
            }

            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

pub(super) fn existing_file(
    path: &Path,
    uid: u32,
    mode: Option<u32>,
) -> Result<Option<File>, InstallationError> {
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("missing parent directory"))?;
    let parent = match open_directory(parent_path, Some(uid)) {
        Ok(parent) => parent,
        Err(InstallationError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }

        Err(error) => return Err(error),
    };
    let file = match open_at(
        &parent,
        &c_name(
            path.file_name()
                .ok_or_else(|| invalid("missing file name"))?,
        )?,
        false,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
        || (metadata.uid() != uid && metadata.uid() != 0)
        || mode.is_some_and(|mode| metadata.mode() & 0o7777 != mode)
    {
        return Err(invalid(
            "existing file has unsafe ownership, type, links or permissions",
        ));
    }

    Ok(Some(file))
}

impl InstallationTransaction {
    pub(super) fn state_directory(
        &mut self,
        path: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), InstallationError> {
        let directory = self.directory(path, uid, gid, 0o700)?;
        let metadata = directory.metadata()?;
        if metadata.uid() != uid || metadata.gid() != gid || metadata.mode() & 0o7777 != 0o700 {
            if fs::read_dir(path)?.next().transpose()?.is_some() {
                return Err(invalid("state directory changed before adoption"));
            }

            self.adopted_directories.push(OriginalDirectory {
                directory: directory.try_clone()?,
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode() & 0o7777,
            });
            set_identity(&directory, uid, gid, 0o700)?;
            directory.sync_all()?;
        }

        Ok(())
    }

    pub(super) fn directory(
        &mut self,
        path: &Path,
        uid: u32,
        gid: u32,
        mode: u32,
    ) -> Result<File, InstallationError> {
        let mut directory = File::open("/")?;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue;
            };
            let name = c_name(name)?;
            directory = match open_at(&directory, &name, true) {
                Ok(child) => {
                    secure_directory(&child, Some(uid))?;
                    child
                }

                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let rollback_parent = directory.try_clone()?;
                    // SAFETY: live directory and valid NUL-terminated child name.
                    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                        return Err(io::Error::last_os_error().into());
                    }

                    self.entries
                        .push(CreatedEntry::created(rollback_parent, name.clone(), true)?);
                    let child = open_at(&directory, &name, true)?;
                    let final_mode = if components.peek().is_none() {
                        mode
                    } else {
                        0o755
                    };
                    set_identity(&child, uid, gid, final_mode)?;
                    directory.sync_all()?;
                    child
                }

                Err(error) => return Err(error.into()),
            };
        }

        Ok(directory)
    }

    pub(super) fn publish(
        &mut self,
        path: &Path,
        source: &mut dyn Read,
        mode: u32,
        uid: u32,
        gid: u32,
        expected_hash: Option<&str>,
    ) -> Result<(), InstallationError> {
        let parent = self.directory(
            path.parent()
                .ok_or_else(|| invalid("missing output parent"))?,
            uid,
            gid,
            0o755,
        )?;
        let name = c_name(
            path.file_name()
                .ok_or_else(|| invalid("missing output name"))?,
        )?;
        let mut random = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let temporary = CString::new(format!(".outrider-install-{}", hex::encode(random)))
            .map_err(|_| invalid("invalid temporary name"))?;
        // Reserve rollback descriptors before creating filesystem entries.
        let temporary_parent = parent.try_clone()?;
        let published_parent = parent.try_clone()?;
        // SAFETY: descriptors and NUL-terminated name are valid; create_new and
        // nofollow prevent replacement or following a hostile existing entry.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }

        // SAFETY: fresh descriptor transfers sole ownership to File.
        let mut destination = unsafe { File::from_raw_fd(descriptor) };
        let temporary_entry = CreatedEntry::created(temporary_parent, temporary.clone(), false)?;
        let device = temporary_entry.device;
        let inode = temporary_entry.inode;
        self.entries.push(temporary_entry);
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut count = 0_u64;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }

            count += read as u64;
            if count > FILE_LIMIT {
                return Err(invalid("source grew beyond package size limit"));
            }

            digest.update(&buffer[..read]);
            destination.write_all(&buffer[..read])?;
        }

        if expected_hash.is_some_and(|hash| hex::encode(digest.finalize()) != hash) {
            return Err(invalid("package changed during installation"));
        }

        set_identity(&destination, uid, gid, mode)?;
        destination.sync_all()?;
        // Publish an already-written owned inode, atomically, without replacing
        // any existing name. linkat is portable to both Linux and macOS.
        // SAFETY: live directory descriptors and NUL-terminated names.
        if unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }

        self.entries.push(CreatedEntry {
            parent: published_parent,
            name,
            directory: false,
            device,
            inode,
        });
        // SAFETY: remove only this transaction's private temporary link.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }

        parent.sync_all()?;
        Ok(())
    }
}

impl Drop for InstallationTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let mut incomplete = false;
        for entry in self.entries.iter().rev() {
            if !entry.still_owned() {
                continue;
            }

            // SAFETY: rollback is restricted to names created by this transaction
            // under held parent descriptors; existing directories are never listed.
            let removed = unsafe {
                libc::unlinkat(
                    entry.parent.as_raw_fd(),
                    entry.name.as_ptr(),
                    if entry.directory {
                        libc::AT_REMOVEDIR
                    } else {
                        0
                    },
                )
            };
            incomplete |= removed != 0;

            incomplete |= entry.parent.sync_all().is_err();
        }

        for original in self.adopted_directories.iter().rev() {
            incomplete |= set_identity(
                &original.directory,
                original.uid,
                original.gid,
                original.mode,
            )
            .is_err();
            incomplete |= original.directory.sync_all().is_err();
        }

        if incomplete {
            eprintln!(
                "outrider installation rollback is incomplete; inspect the requested destinations"
            );
        }
    }
}

impl CreatedEntry {
    fn created(parent: File, name: CString, directory: bool) -> io::Result<Self> {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: metadata is writable and parent/name are valid. No symlink is followed.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: successful fstatat initialized the complete stat value.
        let metadata = unsafe { metadata.assume_init() };
        Ok(Self {
            parent,
            name,
            directory,
            device: metadata.st_dev,
            inode: metadata.st_ino,
        })
    }

    fn still_owned(&self) -> bool {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: metadata is writable and parent/name are valid. No symlink is followed.
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return false;
        }

        // SAFETY: successful fstatat initialized the complete stat value.
        let metadata = unsafe { metadata.assume_init() };
        metadata.st_dev == self.device && metadata.st_ino == self.inode
    }
}

fn set_identity(file: &File, uid: u32, gid: u32, mode: u32) -> Result<(), InstallationError> {
    let metadata = file.metadata()?;
    if metadata.uid() != uid || metadata.gid() != gid {
        // SAFETY: descriptor is live; numeric ids have no pointer preconditions.
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }

    // SAFETY: descriptor is live and mode contains only intended permission bits.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error().into());
    }

    Ok(())
}
