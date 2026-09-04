use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use tempfile::Builder;

/// Replaces `path` without ever exposing a partially written destination.
///
/// The temporary file lives beside the destination so persisting it stays on
/// one filesystem. Its contents are synced before the atomic replacement; on
/// Unix, the parent directory is synced afterward as a best-effort barrier.
///
/// This is the one atomic-replace implementation for every smudgy data file,
/// in core and UI alike; concurrent writers to the same destination are safe
/// against torn output because each write goes through its own uniquely named
/// temporary sibling.
///
/// # Errors
///
/// Returns an I/O error only before the replacement commits (creating, writing,
/// syncing, or persisting the temporary file); the destination is then left
/// untouched. A parent-directory sync failure happens after the replacement
/// and is logged rather than reported: the requested bytes are already
/// authoritative and the caller can no longer roll them back.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |file| file.write_all(contents))
}

fn write_atomic_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let existing_permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };

    let mut temporary = Builder::new()
        .prefix(".smudgy-write-")
        .tempfile_in(parent)?;
    write(temporary.as_file_mut())?;
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|err| err.error)?;
    if let Err(error) = sync_parent(parent) {
        log::warn!(
            "Atomic replacement of {} succeeded, but its parent directory could not be synced: {error}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_new_file_without_leaving_a_temporary_sibling() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("settings.json");

        write_atomic(&path, b"{}").expect("atomic write");

        assert_eq!(fs::read(&path).expect("written file"), b"{}");
        assert_eq!(
            fs::read_dir(dir.path()).expect("directory entries").count(),
            1
        );
    }

    #[test]
    fn replaces_an_existing_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"old").expect("old file");

        write_atomic(&path, b"new").expect("atomic write");

        assert_eq!(fs::read(&path).expect("replaced file"), b"new");
        assert_eq!(
            fs::read_dir(dir.path()).expect("directory entries").count(),
            1
        );
    }

    #[test]
    fn a_failed_write_leaves_the_destination_untouched() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"old").expect("old file");

        let result = write_atomic_with(&path, |_| Err(io::Error::other("injected write failure")));

        assert!(result.is_err());
        assert_eq!(fs::read(&path).expect("original file"), b"old");
        assert_eq!(
            fs::read_dir(dir.path()).expect("directory entries").count(),
            1
        );
    }
}
