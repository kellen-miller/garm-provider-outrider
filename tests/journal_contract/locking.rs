#![cfg(unix)]

use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    sync::{Arc, Barrier},
    thread,
};

use outrider::journal::{AllocationDeleteTarget, Journal, JournalError};

use super::journal_process::{IpcPipe, journal_path};
#[test]
fn dropping_journal_releases_inherited_process_lock() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open first journal owner");
    assert!(matches!(
        Journal::open(&database),
        Err(JournalError::AlreadyRunning)
    ));

    let ready = IpcPipe::open().expect("create child-ready pipe");
    let release = IpcPipe::open().expect("create child-release pipe");
    let child_ready = ready.writer.as_raw_fd();
    let child_release = release.reader.as_raw_fd();
    // SAFETY: the forked child uses only async-signal-safe libc calls before `_exit`.
    let child = unsafe { libc::fork() };
    assert_ne!(
        child,
        -1,
        "fork paused child: {}",
        io::Error::last_os_error()
    );
    if child == 0 {
        let ready_byte = b'R';
        // SAFETY: the inherited pipe descriptor and byte pointer remain valid in the child.
        if unsafe { libc::write(child_ready, (&ready_byte as *const u8).cast(), 1) } != 1 {
            // SAFETY: `_exit` terminates the child without running inherited Rust destructors.
            unsafe { libc::_exit(1) };
        }

        let mut release_byte = 0_u8;
        // SAFETY: the inherited pipe descriptor and byte pointer remain valid in the child.
        if unsafe { libc::read(child_release, (&mut release_byte as *mut u8).cast(), 1) } != 1 {
            // SAFETY: `_exit` terminates the child without running inherited Rust destructors.
            unsafe { libc::_exit(1) };
        }

        // SAFETY: `_exit` terminates the child without running inherited Rust destructors.
        unsafe { libc::_exit(0) };
    }
    drop(ready.writer);
    drop(release.reader);

    let mut ready_poll = libc::pollfd {
        fd: ready.reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `ready_poll` points to one initialized poll descriptor.
    let ready_result = unsafe { libc::poll(&mut ready_poll, 1, 3_000) };
    let ready_observed = if ready_result == 1 && ready_poll.revents & libc::POLLIN != 0 {
        let mut byte = [0; 1];
        (&ready.reader).read_exact(&mut byte).is_ok() && byte == *b"R"
    } else {
        false
    };

    drop(journal);
    let replacement = Journal::open(&database);

    let release_byte = b'X';
    // SAFETY: the parent owns the write end and the byte pointer is valid.
    let release_result = unsafe {
        libc::write(
            release.writer.as_raw_fd(),
            (&release_byte as *const u8).cast(),
            1,
        )
    };
    if release_result != 1 {
        // SAFETY: `child` is the live PID returned by `fork`.
        unsafe { libc::kill(child, libc::SIGKILL) };
    }
    let mut child_status = 0;
    // SAFETY: `child` is the PID returned by `fork` and `child_status` is writable.
    let wait_result = unsafe { libc::waitpid(child, &mut child_status, 0) };
    let replacement_acquired = replacement.is_ok();
    drop(replacement);
    assert!(
        std::fs::remove_file(&database).is_ok(),
        "remove test database"
    );
    assert!(
        std::fs::remove_file(database.with_extension("lock")).is_ok(),
        "remove test lock file"
    );

    assert!(ready_observed, "child did not reach the pre-exec pause");
    assert_eq!(release_result, 1, "release child from pre-exec pause");
    assert_eq!(wait_result, child, "wait for paused child");
    assert!(
        libc::WIFEXITED(child_status) && libc::WEXITSTATUS(child_status) == 0,
        "paused child failed with status {child_status}"
    );
    assert!(
        replacement_acquired,
        "replacement journal should acquire the lock after the old owner drops"
    );
}

#[test]
fn first_open_rejects_dangling_database_symlink_without_creating_target() {
    let alias = journal_path();
    let target = alias.with_extension("target.sqlite3");
    std::os::unix::fs::symlink(&target, &alias).expect("create dangling database alias");

    assert!(
        Journal::open(&alias).is_err(),
        "refuse unresolved database symlink"
    );
    assert!(!target.exists(), "do not create dangling symlink target");
    assert!(
        !alias.with_extension("lock").exists(),
        "do not create alias lock sidecar"
    );
    assert!(
        !target.with_extension("lock").exists(),
        "do not create target lock sidecar"
    );
    std::fs::remove_file(alias).expect("remove dangling database alias");
}

#[test]
fn journal_open_error_releases_process_lock() {
    let database = journal_path();
    std::fs::create_dir(&database).expect("create directory at database path");

    assert!(Journal::open(&database).is_err());
    assert!(
        std::fs::remove_dir(&database).is_ok(),
        "remove database directory"
    );

    let journal = Journal::open(&database).expect("open after startup error");
    drop(journal);
    assert!(
        std::fs::remove_file(&database).is_ok(),
        "remove test database"
    );
    assert!(
        std::fs::remove_file(database.with_extension("lock")).is_ok(),
        "remove test lock file"
    );
}

#[test]
fn delete_fence_blocks_name_and_opaque_id_across_restart() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open journal");
    assert!(matches!(
        journal
            .allocation_delete_target("controller-a", "runner-a", Some("pool-a"))
            .expect("record missing-name fence"),
        AllocationDeleteTarget::Absent
    ));
    assert!(matches!(
        journal
            .allocation_delete_target("controller-a", "opaque-fenced-id", Some("pool-a"))
            .expect("record missing-ID fence"),
        AllocationDeleteTarget::Absent
    ));
    drop(journal);

    let journal = Journal::open(&database).expect("reopen journal");
    assert!(matches!(
        journal.reserve_allocation(
            "new-opaque-id",
            "controller-a",
            "runner-a",
            "snapshot",
            "pool-a"
        ),
        Err(JournalError::AllocationIdentityDeleted)
    ));
    assert!(matches!(
        journal.reserve_allocation(
            "opaque-fenced-id",
            "controller-a",
            "different-runner",
            "snapshot",
            "pool-a"
        ),
        Err(JournalError::AllocationIdentityDeleted)
    ));
    assert!(
        journal
            .reserve_allocation(
                "other-controller-id",
                "controller-b",
                "runner-a",
                "snapshot",
                "pool-a"
            )
            .expect("controller scope isolates delete fence")
            .1
    );
    drop(journal);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn concurrent_absent_delete_and_reservation_have_one_durable_order() {
    let database = journal_path();
    let journal = Arc::new(Journal::open(&database).expect("open journal"));
    let barrier = Arc::new(Barrier::new(3));
    let reserve_journal = Arc::clone(&journal);
    let reserve_barrier = Arc::clone(&barrier);
    let reservation = thread::spawn(move || {
        reserve_barrier.wait();
        reserve_journal.reserve_allocation(
            "opaque-race-id",
            "controller-race",
            "runner-race",
            "snapshot",
            "pool",
        )
    });
    let delete_journal = Arc::clone(&journal);
    let delete_barrier = Arc::clone(&barrier);
    let deletion = thread::spawn(move || {
        delete_barrier.wait();
        delete_journal.allocation_delete_target("controller-race", "runner-race", Some("pool"))
    });
    barrier.wait();
    let reserved = reservation.join().expect("reservation worker");
    let deleted = deletion.join().expect("delete worker");
    match (reserved, deleted) {
        (Ok((_, true)), Ok(AllocationDeleteTarget::Owned(_))) => {}
        (Err(JournalError::AllocationIdentityDeleted), Ok(AllocationDeleteTarget::Absent)) => {}
        (reserve, delete) => panic!("unexpected reserve/delete ordering: {reserve:?}, {delete:?}"),
    }
    drop(journal);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}
