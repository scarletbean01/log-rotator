//! File identity and reverse-seek tail reading.
//!
//! Pure `std`, no tokio. Reverse tail reads a fixed number of trailing lines
//! from a file without loading the whole file: it scans backward in bounded
//! chunks, counting newlines with SIMD-accelerated `memchr::memrchr`.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use memchr::{memchr, memrchr};

/// Stable identity of an open file or a path (device + inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    pub fn of_path(path: &Path) -> io::Result<Self> {
        let md = std::fs::metadata(path)?;
        Ok(Self {
            dev: md.dev(),
            ino: md.ino(),
        })
    }

    pub fn of_file(file: &File) -> io::Result<Self> {
        let md = file.metadata()?;
        Ok(Self {
            dev: md.dev(),
            ino: md.ino(),
        })
    }
}

/// The last N lines of a file plus the byte range they occupy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailWindow {
    pub start_offset: u64,
    pub end_offset: u64,
    pub lines: Vec<String>,
    pub anchor_line: Option<usize>,
}

/// Read the last `lines` lines of `path` (byte-offset-accurate window).
///
/// Opens with `O_RDONLY` and no advisory locks, so concurrent writers are
/// never blocked. Memory usage is bounded by the window size, not file size.
pub fn read_tail(path: &Path, lines: usize, chunk_size: usize) -> io::Result<TailWindow> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();

    if size == 0 || lines == 0 {
        return Ok(TailWindow {
            start_offset: 0,
            end_offset: size,
            lines: Vec::new(),
            anchor_line: None,
        });
    }

    // Does the file end with a newline? Determines whether the final segment
    // is a terminated line (no trailing empty line) or not.
    let mut last = [0u8; 1];
    file.read_exact_at(&mut last, size - 1)?;
    let ends_nl = last[0] == b'\n';

    // To expose the last `lines` lines we stop after counting this many
    // newlines backward (one extra when the file ends in '\n', because that
    // final newline terminates the last line rather than starting a new one).
    let target = lines + usize::from(ends_nl);

    let mut newline_count = 0usize;
    let mut pos = size;
    let mut start = 0u64;
    let mut found = false;
    let mut chunk = vec![0u8; chunk_size];

    'outer: while pos > 0 {
        let read_start = pos.saturating_sub(chunk_size as u64);
        let read_len = (pos - read_start) as usize;
        file.read_exact_at(&mut chunk[..read_len], read_start)?;

        let mut sub = &chunk[..read_len];
        while let Some(i) = memrchr(b'\n', sub) {
            newline_count += 1;
            if newline_count == target {
                start = read_start + i as u64 + 1;
                found = true;
                break 'outer;
            }
            sub = &sub[..i];
        }

        pos = read_start;
    }

    // `found` is false only when the file has fewer than `target` newlines,
    // i.e. fewer than `lines` lines; in that case the whole file is the tail.
    if !found {
        start = 0;
    }

    // Cap the window in bytes: a file with fewer (or pathologicaly long)
    // lines must never turn into a whole-file read. The first line of a
    // capped window may be partial (`tail -c` semantics).
    let floor = size.saturating_sub(crate::limits::MAX_TAIL_WINDOW_BYTES);
    if start < floor {
        start = floor;
    }

    let bytes = read_range(&file, start, size)?;
    let decoded = String::from_utf8_lossy(&bytes);
    let mut out: Vec<String> = decoded.split('\n').map(str::to_string).collect();
    // Drop the empty segment produced by a trailing newline.
    if out.last().is_some_and(String::is_empty) {
        out.pop();
    }

    Ok(TailWindow {
        start_offset: start,
        end_offset: size,
        lines: out,
        anchor_line: None,
    })
}

/// Helper: scans backward from `from` down to `min_bound`, counting newlines.
///
/// Returns `(start_offset, count)`. If `target` newlines are found,
/// `start_offset` is the byte immediately following the `target`-th newline.
/// If `min_bound` is reached first, `start_offset` is `min_bound`.
fn scan_newlines_backward(
    file: &File,
    from: u64,
    min_bound: u64,
    target: usize,
    chunk_size: usize,
) -> io::Result<(u64, usize)> {
    if target == 0 || from <= min_bound {
        return Ok((from, 0));
    }
    let mut pos = from;
    let mut count = 0usize;
    let mut chunk = vec![0u8; chunk_size];

    while pos > min_bound {
        let read_start = pos.saturating_sub(chunk_size as u64).max(min_bound);
        let read_len = (pos - read_start) as usize;
        if read_len == 0 {
            break;
        }
        file.read_exact_at(&mut chunk[..read_len], read_start)?;
        let mut sub = &chunk[..read_len];
        while let Some(i) = memrchr(b'\n', sub) {
            count += 1;
            if count == target {
                return Ok((read_start + i as u64 + 1, count));
            }
            sub = &sub[..i];
        }
        pos = read_start;
    }

    Ok((min_bound, count))
}

/// Read N lines centred on `anchor_offset` (the byte offset of a matched line).
///
/// Walks backward from `anchor_offset` to find the start of `lines / 2`
/// preceding lines, then reads forward until `lines` total lines are collected.
/// Memory is strictly bounded by `MAX_TAIL_WINDOW_BYTES` and `chunk_size`.
pub fn read_around(
    path: &Path,
    anchor_offset: u64,
    lines: usize,
    chunk_size: usize,
) -> io::Result<TailWindow> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    if size == 0 || lines == 0 {
        return Ok(TailWindow {
            start_offset: 0,
            end_offset: size,
            lines: Vec::new(),
            anchor_line: None,
        });
    }

    let anchor = anchor_offset.min(size);
    let before = lines / 2;
    let min_start = anchor.saturating_sub(crate::limits::MAX_TAIL_WINDOW_BYTES / 2);
    let chunk_size = chunk_size.max(1);

    // 1. Walk backward from `anchor` to find `before` preceding lines.
    // If before == 0 (e.g. lines == 1), target is 1 (the newline terminating the preceding line).
    let (mut start, _) = scan_newlines_backward(&file, anchor, min_start, before + 1, chunk_size)?;

    // 2. Read forward from `start` in bounded `chunk_size` steps until `lines`
    //    newlines are collected, EOF is reached, or the window cap is hit.
    //    Memory scales with the actual window, never MAX_TAIL_WINDOW_BYTES.
    let mut bytes: Vec<u8> = Vec::new();
    let mut newline_count = 0usize;
    let mut pos = start;
    let window_end = start
        .saturating_add(crate::limits::MAX_TAIL_WINDOW_BYTES)
        .min(size);
    while pos < window_end && newline_count < lines {
        let end = (pos + chunk_size as u64).min(window_end);
        let chunk = read_range(&file, pos, end)?;
        if chunk.is_empty() {
            break; // file shrank mid-read; treat whatever we have as the window
        }
        let mut consumed = chunk.len();
        let mut sub = &chunk[..];
        let mut sub_offset = 0usize;
        while let Some(i) = memchr(b'\n', sub) {
            newline_count += 1;
            if newline_count == lines {
                consumed = sub_offset + i + 1;
                break;
            }
            sub_offset += i + 1;
            sub = &sub[i + 1..];
        }
        bytes.extend_from_slice(&chunk[..consumed]);
        pos += consumed as u64;
    }

    // An unterminated final line at EOF still counts as a line.
    let ends_with_nl = bytes.last() == Some(&b'\n');
    let effective_lines = newline_count + usize::from(!ends_with_nl && !bytes.is_empty());

    if newline_count < lines && pos >= size && start > 0 {
        // 3. Near EOF: backfill missing lines by expanding backward from `start`.
        let missing = lines.saturating_sub(effective_lines);
        if missing > 0 {
            let backfill_min = start.saturating_sub(
                crate::limits::MAX_TAIL_WINDOW_BYTES.saturating_sub(bytes.len() as u64),
            );
            if backfill_min < start {
                // `start` sits right after a newline; the 1st newline found
                // backward is that one, so target `missing + 1` yields
                // exactly `missing` additional lines.
                let (new_start, _) =
                    scan_newlines_backward(&file, start, backfill_min, missing + 1, chunk_size)?;
                if new_start < start {
                    let prefix = read_range(&file, new_start, start)?;
                    let mut full = prefix;
                    full.extend_from_slice(&bytes);
                    bytes = full;
                    start = new_start;
                }
            }
        }
    } else if newline_count < lines && pos < size {
        // 4. Window hit MAX_TAIL_WINDOW_BYTES: trim trailing partial line if it doesn't end in '\n'.
        if let Some(last_nl) = memrchr(b'\n', &bytes) {
            let candidate_len = last_nl + 1;
            if start + candidate_len as u64 >= anchor {
                bytes.truncate(candidate_len);
            }
        }
    }

    let actual_end = start + bytes.len() as u64;
    let decoded = String::from_utf8_lossy(&bytes);
    let mut out: Vec<String> = decoded.split('\n').map(str::to_string).collect();
    if out.last().is_some_and(String::is_empty) {
        out.pop();
    }

    // Determine the exact line index containing `anchor_offset` using raw byte offsets.
    let mut anchor_line = None;
    if !out.is_empty() && anchor_offset >= start {
        let mut line_start = start;
        let mut raw_slice = &bytes[..];
        for (i, _) in out.iter().enumerate() {
            let line_len = match memchr(b'\n', raw_slice) {
                Some(nl) => nl + 1,
                None => raw_slice.len(),
            };
            let line_end = line_start + line_len as u64;
            if anchor_offset >= line_start
                && (anchor_offset < line_end || (i + 1 == out.len() && anchor_offset <= line_end))
            {
                anchor_line = Some(i);
                break;
            }
            line_start = line_end;
            if line_len <= raw_slice.len() {
                raw_slice = &raw_slice[line_len..];
            } else {
                break;
            }
        }
    }

    Ok(TailWindow {
        start_offset: start,
        end_offset: actual_end,
        lines: out,
        anchor_line,
    })
}

/// Read `[start, end)` into memory, tolerant of a short read at EOF.
fn read_range(file: &File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        let n = file.read_at(&mut buf[filled..], start + filled as u64)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fn write_file(path: &Path, content: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(content).unwrap();
    }

    fn tail(path: &Path, lines: usize) -> TailWindow {
        read_tail(path, lines, 64).unwrap()
    }

    #[test]
    fn empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"");
        let w = tail(&p, 10);
        assert_eq!(w.lines, Vec::<String>::new());
        assert_eq!(w.start_offset, 0);
        assert_eq!(w.end_offset, 0);
    }

    #[test]
    fn fewer_lines_than_requested() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"one\ntwo\n");
        let w = tail(&p, 10);
        assert_eq!(w.lines, vec!["one", "two"]);
        assert_eq!(w.start_offset, 0);
    }

    #[test]
    fn exact_requested_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"a\nb\nc\nd\n");
        let w = tail(&p, 2);
        assert_eq!(w.lines, vec!["c", "d"]);
        assert_eq!(w.start_offset, 4);
        assert_eq!(w.end_offset, 8);
    }

    #[test]
    fn no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"a\nb\nc");
        let w = tail(&p, 2);
        assert_eq!(w.lines, vec!["b", "c"]);
        assert_eq!(w.start_offset, 2);
    }

    #[test]
    fn single_line_no_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"hello world");
        let w = tail(&p, 5);
        assert_eq!(w.lines, vec!["hello world"]);
        assert_eq!(w.start_offset, 0);
    }

    #[test]
    fn line_spanning_multiple_chunks() {
        // 200 'A's = ~3.1 chunks of 64 bytes, one long line.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = vec![b'A'; 200];
        content.push(b'\n');
        write_file(&p, &content);
        let w = tail(&p, 1);
        assert_eq!(w.lines.len(), 1);
        assert_eq!(w.lines[0], "A".repeat(200));
        assert_eq!(w.start_offset, 0);
    }

    #[test]
    fn window_capped_for_single_huge_line() {
        // One 6 MiB line with no newline: the window must be capped at
        // MAX_TAIL_WINDOW_BYTES instead of reading the whole file.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let content = vec![b'A'; 6 * 1024 * 1024];
        write_file(&p, &content);
        let w = tail(&p, 5);
        assert_eq!(w.lines.len(), 1);
        assert_eq!(
            w.start_offset,
            6 * 1024 * 1024 - crate::limits::MAX_TAIL_WINDOW_BYTES
        );
        assert_eq!(w.end_offset, 6 * 1024 * 1024);
        assert!(w.end_offset - w.start_offset <= crate::limits::MAX_TAIL_WINDOW_BYTES);
        assert_eq!(
            w.lines[0].len(),
            crate::limits::MAX_TAIL_WINDOW_BYTES as usize
        );
    }

    #[test]
    fn chunk_boundary_newline() {
        // A newline exactly at offset 64 (the first chunk boundary).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = vec![b'x'; 64];
        content.push(b'\n');
        content.extend_from_slice(b"tail\n");
        write_file(&p, &content);
        let w = tail(&p, 1);
        assert_eq!(w.lines, vec!["tail"]);
        // start_offset = 65 (after the newline at offset 64)
        assert_eq!(w.start_offset, 65);
        assert_eq!(w.end_offset, 70);
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        // Last line carries invalid UTF-8 bytes; must not panic, just lossy.
        let content = vec![b'a', b'\n', 0xff, 0xfe];
        write_file(&p, &content);
        let w = tail(&p, 1);
        assert_eq!(w.lines.len(), 1);
        assert!(w.lines[0].contains('\u{fffd}'));
    }

    #[test]
    fn many_lines_scanned_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = String::new();
        for i in 0..1000 {
            content.push_str(&format!("line {i:04}\n"));
        }
        write_file(&p, content.as_bytes());
        let w = tail(&p, 3);
        assert_eq!(w.lines, vec!["line 0997", "line 0998", "line 0999"]);
        // start_offset points at the first byte of "line 0997".
        let expected_start = (content.len() - "line 0997\nline 0998\nline 0999\n".len()) as u64;
        assert_eq!(w.start_offset, expected_start);
        assert_eq!(w.end_offset, content.len() as u64);
    }

    #[test]
    fn empty_line_handling() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"a\n\nb\n");
        let w = tail(&p, 3);
        assert_eq!(w.lines, vec!["a", "", "b"]);
    }

    #[test]
    fn read_around_centred() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i:02}\n"));
        }
        write_file(&p, content.as_bytes());

        // "line 05" starts at offset 5 * 8 = 40.
        let w = read_around(&p, 40, 4, 16).unwrap();
        assert_eq!(w.lines, vec!["line 03", "line 04", "line 05", "line 06"]);
        assert_eq!(w.anchor_line, Some(2));
        assert_eq!(w.lines[2], "line 05");
    }

    #[test]
    fn read_around_lines_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i:02}\n"));
        }
        write_file(&p, content.as_bytes());

        // "line 05" starts at offset 40. With lines=1, must return line 05, NOT line 00.
        let w = read_around(&p, 40, 1, 16).unwrap();
        assert_eq!(w.lines, vec!["line 05"]);
        assert_eq!(w.anchor_line, Some(0));

        // Anchor in the middle of line 05 (offset 43)
        let w_mid = read_around(&p, 43, 1, 16).unwrap();
        assert_eq!(w_mid.lines, vec!["line 05"]);
        assert_eq!(w_mid.anchor_line, Some(0));
    }

    #[test]
    fn read_around_eof_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"line 00\nline 01\nline 02\n");

        // Around line 00 (start of file): requesting 2 lines
        let w_start = read_around(&p, 0, 2, 16).unwrap();
        assert_eq!(w_start.lines, vec!["line 00", "line 01"]);
        assert_eq!(w_start.anchor_line, Some(0));

        // Around line 02 (end of file): requesting 3 lines.
        // Must backfill line 00 so all 3 lines are returned!
        let w_end = read_around(&p, 16, 3, 16).unwrap();
        assert_eq!(w_end.lines, vec!["line 00", "line 01", "line 02"]);
        assert_eq!(w_end.anchor_line, Some(2));
    }

    #[test]
    fn read_around_lossy_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        // Line with invalid UTF-8 bytes before anchor line
        let mut content = vec![b'a', b'\n', 0xff, 0xfe, b'\n'];
        content.extend_from_slice(b"target line\n");
        write_file(&p, &content);

        // "target line" starts at offset 5.
        let w = read_around(&p, 5, 3, 16).unwrap();
        assert_eq!(w.lines.len(), 3);
        assert_eq!(w.anchor_line, Some(2));
        assert_eq!(w.lines[2], "target line");
    }

    #[test]
    fn read_around_window_capped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        // 5 MiB of data before anchor line
        let mut content = vec![b'x'; 5 * 1024 * 1024];
        content.push(b'\n');
        let anchor = content.len() as u64;
        content.extend_from_slice(b"anchor line\n");
        content.extend_from_slice(&vec![b'y'; 1024 * 1024]);
        write_file(&p, &content);

        let w = read_around(&p, anchor, 5, 64).unwrap();
        // Window must be capped at MAX_TAIL_WINDOW_BYTES and contain anchor line
        assert!(w.end_offset - w.start_offset <= crate::limits::MAX_TAIL_WINDOW_BYTES);
        assert!(w.start_offset <= anchor);
        assert!(w.end_offset >= anchor + "anchor line\n".len() as u64);
        let idx = w.lines.iter().position(|l| l == "anchor line").unwrap();
        assert_eq!(w.anchor_line, Some(idx));
    }

    #[test]
    fn read_around_unterminated_last_line_at_eof() {
        // Actively written file: the last line has no trailing '\n'.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"line 00\nline 01\nline 02");

        // "line 02" starts at offset 16. Requesting 2 lines must NOT
        // over-backfill and return 3.
        let w = read_around(&p, 16, 2, 16).unwrap();
        assert_eq!(w.lines, vec!["line 01", "line 02"]);
        assert_eq!(w.anchor_line, Some(1));

        // lines=1: just the anchor line, no backfill at all.
        let w1 = read_around(&p, 16, 1, 16).unwrap();
        assert_eq!(w1.lines, vec!["line 02"]);
        assert_eq!(w1.anchor_line, Some(0));
    }

    #[test]
    fn read_around_eof_backfill_is_exact() {
        // Terminated EOF: backfill must pull exactly the missing lines,
        // never one extra.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"pre\nline 00\nline 01\nline 02\n");

        // "line 02" starts at offset 20; window covers line 01..line 02,
        // backfill must add exactly line 00 — not "pre".
        let w = read_around(&p, 20, 3, 16).unwrap();
        assert_eq!(w.lines, vec!["line 00", "line 01", "line 02"]);
        assert_eq!(w.anchor_line, Some(2));
    }
}
