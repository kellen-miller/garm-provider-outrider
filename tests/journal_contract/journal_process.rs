#![cfg(unix)]

use std::{
    fs::File,
    io::{self},
    os::fd::FromRawFd,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_JOURNAL: AtomicUsize = AtomicUsize::new(0);

pub(super) struct IpcPipe {
    pub(super) reader: File,
    pub(super) writer: File,
}

impl IpcPipe {
    pub(super) fn open() -> io::Result<Self> {
        let mut descriptors = [0; 2];
        // SAFETY: `descriptors` points to two writable integers for `pipe`.
        if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            // SAFETY: successful `pipe` initialized this owned descriptor.
            reader: unsafe { File::from_raw_fd(descriptors[0]) },
            // SAFETY: successful `pipe` initialized this distinct owned descriptor.
            writer: unsafe { File::from_raw_fd(descriptors[1]) },
        })
    }
}

pub(super) fn journal_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_JOURNAL.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "outrider-journal-{}-{timestamp}-{sequence}.sqlite3",
        std::process::id()
    ))
}

pub(super) fn unix_seconds() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}
