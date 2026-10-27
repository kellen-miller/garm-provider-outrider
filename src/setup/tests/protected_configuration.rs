use super::ProtectedDirectory;
use crate::setup::{SetupError, configuration::ConfigurationFile};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

#[test]
fn rejects_relative_symlink_and_hardlinked_configuration() {
    let directory = ProtectedDirectory::new();
    let path = directory.file("config.toml", "version = 1\n");
    assert!(matches!(
        ConfigurationFile::open(Path::new("config.toml")),
        Err(SetupError::ProtectedConfiguration)
    ));
    let link = directory.path().join("symlink.toml");
    symlink(&path, &link).unwrap();
    assert!(matches!(
        ConfigurationFile::open(&link),
        Err(SetupError::ProtectedConfiguration)
    ));
    let hardlink = directory.path().join("hardlink.toml");
    fs::hard_link(&path, &hardlink).unwrap();
    for path in [&path, &hardlink] {
        assert!(matches!(
            ConfigurationFile::open(path),
            Err(SetupError::ProtectedConfiguration)
        ));
    }
}

#[test]
fn rejects_writable_and_symlink_ancestors() {
    let directory = ProtectedDirectory::new();
    let child = directory.path().join("child");
    fs::create_dir(&child).unwrap();
    let path = child.join("config.toml");
    fs::write(&path, "version = 1\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    for mode in [0o770, 0o707] {
        fs::set_permissions(&child, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(
            ConfigurationFile::open(&path),
            Err(SetupError::ProtectedConfiguration)
        ));
    }

    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    let link = directory.path().join("linked-directory");
    symlink(&child, &link).unwrap();
    assert!(matches!(
        ConfigurationFile::open(&link.join("config.toml")),
        Err(SetupError::ProtectedConfiguration)
    ));
}

#[test]
fn rejects_unprotected_mode_nonregular_and_oversized_files() {
    let directory = ProtectedDirectory::new();
    let path = directory.file("config.toml", "version = 1\n");
    for mode in [0o644, 0o620, 0o610, 0o601] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(
            ConfigurationFile::open(&path),
            Err(SetupError::ProtectedConfiguration)
        ));
    }

    assert!(matches!(
        ConfigurationFile::open(directory.path()),
        Err(SetupError::ProtectedConfiguration)
    ));
    let oversized = directory.file("oversized.toml", vec![b'x'; 1_048_577]);
    assert!(matches!(
        ConfigurationFile::open(&oversized),
        Err(SetupError::ProtectedConfiguration)
    ));
}

#[test]
fn publication_preserves_owner_group_mode_and_repeat_inode() {
    let directory = ProtectedDirectory::new();
    let path = directory.file("config.toml", "# retained original\nversion = 1\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&path).unwrap();
    let configuration = ConfigurationFile::open(&path).unwrap();
    assert!(!configuration.publish(configuration.bytes()).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().ino(), before.ino());
    assert!(
        configuration
            .publish(b"# qualified\nversion = 1\n")
            .unwrap()
    );
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.uid(), before.uid());
    assert_eq!(after.gid(), before.gid());
    assert_eq!(after.mode() & 0o777, before.mode() & 0o777);
    assert_eq!(fs::read(&path).unwrap(), b"# qualified\nversion = 1\n");
    let repeat = ConfigurationFile::open(&path).unwrap();
    assert!(!repeat.publish(repeat.bytes()).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().ino(), after.ino());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn publication_rejects_in_place_and_replaced_concurrent_edits() {
    let directory = ProtectedDirectory::new();
    for replace in [false, true] {
        let path = directory.file("config.toml", "original\n");
        let configuration = ConfigurationFile::open(&path).unwrap();
        if replace {
            let replacement = directory.file("replacement.toml", "original\n");
            fs::rename(replacement, &path).unwrap();
        } else {
            fs::write(&path, "concurrent\n").unwrap();
        }

        let concurrent = fs::read(&path).unwrap();
        assert!(matches!(
            configuration.publish(b"qualified\n"),
            Err(SetupError::ConcurrentEdit)
        ));
        assert_eq!(fs::read(&path).unwrap(), concurrent);
    }
}

#[test]
fn unchanged_publication_still_rejects_concurrent_replacement() {
    let directory = ProtectedDirectory::new();
    let path = directory.file("config.toml", "original\n");
    let configuration = ConfigurationFile::open(&path).unwrap();
    let replacement = directory.file("replacement.toml", "concurrent\n");
    fs::rename(replacement, &path).unwrap();
    assert!(matches!(
        configuration.publish(configuration.bytes()),
        Err(SetupError::ConcurrentEdit)
    ));
    assert_eq!(fs::read(&path).unwrap(), b"concurrent\n");
}

#[test]
fn publication_rejects_symlink_replacement_without_touching_target() {
    let directory = ProtectedDirectory::new();
    let path = directory.file("config.toml", "original\n");
    let configuration = ConfigurationFile::open(&path).unwrap();
    let target = directory.file("target.toml", "independent\n");
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    assert!(configuration.publish(b"qualified\n").is_err());
    assert_eq!(fs::read(&target).unwrap(), b"independent\n");
}

#[test]
fn publication_rejects_new_hardlink_or_insecure_permissions() {
    let directory = ProtectedDirectory::new();
    for hardlink in [false, true] {
        let path = directory.file("config.toml", "original\n");
        let configuration = ConfigurationFile::open(&path).unwrap();
        if hardlink {
            fs::hard_link(&path, directory.path().join("other.toml")).unwrap();
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        }

        assert!(matches!(
            configuration.publish(b"qualified\n"),
            Err(SetupError::ProtectedConfiguration | SetupError::ConcurrentEdit)
        ));
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
    }
}
