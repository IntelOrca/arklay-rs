//! Atomic single-file output.
//!
//! A command that writes one artifact (a capture, an assembled script, a
//! pack) writes a sibling temporary file and renames it over the destination
//! once the bytes are synced to disk. A failure mid-write removes the
//! temporary file and leaves any previous destination untouched, so a failed
//! run never leaves a partial artifact behind.
//!
//! The rename stays within the destination's directory, so it is atomic on
//! every supported platform and does not require a temporary directory on the
//! same volume.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The sibling temporary path for `path`.
///
/// The name keeps the destination's file name so two commands writing
/// different files in the same directory cannot collide, and carries the
/// process id so concurrent processes cannot collide either.
pub fn temp_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name to write", path.display()))?;
    let mut temp = name.to_os_string();
    temp.push(format!(".{}.tmp", std::process::id()));
    Ok(path.with_file_name(temp))
}

/// Write `contents` to `path` atomically: temporary sibling, then rename.
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    let temp = temp_path(path)?;
    let mut file = std::fs::File::create(&temp)
        .with_context(|| format!("failed to create {}", temp.display()))?;
    let result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", temp.display()));
    drop(file);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(err).with_context(|| format!("failed to write {}", path.display()));
    }
    std::fs::rename(&temp, path).with_context(|| {
        let _ = std::fs::remove_file(&temp);
        format!(
            "failed to move {} into place at {}",
            temp.display(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-atomic-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_replaces_the_destination_and_leaves_no_temporary() {
        let dir = TempDir::new("write");
        let path = dir.0.join("out.bin");
        std::fs::write(&path, b"old").unwrap();

        write(&path, b"new contents").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new contents");
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    #[test]
    fn write_creates_a_new_destination() {
        let dir = TempDir::new("create");
        let path = dir.0.join("out.bin");
        write(&path, b"contents").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"contents");
    }

    #[test]
    fn a_failed_write_keeps_the_old_destination_and_removes_the_temporary() {
        let dir = TempDir::new("failure");
        let path = dir.0.join("out.bin");
        std::fs::write(&path, b"old").unwrap();
        // A destination that is a non-empty directory makes the final rename
        // fail after the temporary was written.
        let blocked = dir.0.join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();

        let err = write(&blocked, b"new").unwrap_err();

        let text = format!("{err:#}");
        assert!(text.contains("blocked"), "{text}");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    #[test]
    fn temp_path_is_a_sibling_of_the_destination() {
        let path = Path::new("/tmp/some/dir/capture.bmp");
        let temp = temp_path(path).unwrap();
        assert_eq!(temp.parent(), path.parent());
        assert!(
            temp.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("capture.bmp.")
        );
    }
}
