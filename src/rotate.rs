//! Log-rotation and truncation detection.
//!
//! `check` compares the identity (dev+inode) of an already-open file handle
//! against the path it came from, and the path's current length against the
//! offset we last read. It distinguishes the two common rotation strategies:
//! move-and-create (`Rotated`) and `copytruncate` (`Truncated`).

use std::fs::File;
use std::io;
use std::path::Path;

use crate::logfile::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationEvent {
    /// The file was moved away and a new one created at the same path.
    Rotated,
    /// The file was truncated in place (content removed, inode unchanged).
    Truncated,
}

/// Detect rotation/truncation relative to an open handle and last-read offset.
///
/// `prev_size` is the largest path length observed so far; a shrink below it
/// is treated as copytruncate even when the file has regrown past
/// `last_offset` (possible while replaying an old offset). Known limitation:
/// a truncate + regrow *beyond* `prev_size` between two checks is
/// indistinguishable from an append and cannot be detected by size alone.
pub fn check(
    open: &File,
    path: &Path,
    last_offset: u64,
    prev_size: u64,
) -> io::Result<Option<RotationEvent>> {
    let open_id = FileId::of_file(open)?;

    match FileId::of_path(path) {
        // Path now points at a different inode than the one we hold open.
        Ok(path_id) if path_id != open_id => return Ok(Some(RotationEvent::Rotated)),
        Ok(_) => {}
        // Path gone entirely: the file was moved away (new one may not exist yet).
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(RotationEvent::Rotated));
        }
        Err(e) => return Err(e),
    }

    let len = path.metadata()?.len();
    if len < last_offset || len < prev_size {
        return Ok(Some(RotationEvent::Truncated));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fn open_write(path: &Path, content: &[u8]) -> File {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(content).unwrap();
        f
    }

    #[test]
    fn no_event_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let f = open_write(&p, b"hello\n");
        assert_eq!(check(&f, &p, 6, 6).unwrap(), None);
    }

    #[test]
    fn rotated_inode_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let f = open_write(&p, b"hello\n");

        // move-and-create
        std::fs::rename(&p, dir.path().join("log.1")).unwrap();
        open_write(&p, b"new\n");

        assert_eq!(check(&f, &p, 6, 6).unwrap(), Some(RotationEvent::Rotated));
    }

    #[test]
    fn truncated_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let f = open_write(&p, b"hello world\n");
        let last_offset = 12;
        let prev_size = 12;
        let t = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        t.set_len(0).unwrap();

        assert_eq!(
            check(&f, &p, last_offset, prev_size).unwrap(),
            Some(RotationEvent::Truncated)
        );
    }

    #[test]
    fn regrow_below_prev_size_detected() {
        // Replay scenario: the reader is at offset 4 while the file is 16
        // bytes; the writer truncates and regrows to 8 bytes — below the
        // previously observed size but past `last_offset`. Size-vs-offset
        // alone would miss this.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let f = open_write(&p, b"0123456789abcdef");
        open_write(&p, b"abcdefgh");
        assert_eq!(
            check(&f, &p, 4, 16).unwrap(),
            Some(RotationEvent::Truncated)
        );
    }
    #[test]
    fn moved_away_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let f = open_write(&p, b"hello\n");
        std::fs::rename(&p, dir.path().join("gone")).unwrap();
        assert_eq!(check(&f, &p, 6, 6).unwrap(), Some(RotationEvent::Rotated));
    }
}
