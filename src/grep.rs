//! Grep engine: linear-time matching over arbitrarily large files.
//!
//! Sync + channel: the caller runs `grep` on the blocking pool and consumes
//! `GrepHit`s from the channel. The channel is bounded, so a slow consumer
//! applies backpressure (the disk reader blocks on `blocking_send`); a dropped
//! receiver is the cancellation signal (the first failed send returns early).
//!
//! Lines longer than [`crate::limits::MAX_LINE_BYTES`] are skipped entirely —
//! never assembled or matched — so RSS stays bounded on pathological files.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;

use memchr::memmem;
use memchr::{memchr, memrchr};

use crate::limits::MAX_LINE_BYTES;

#[derive(Debug)]
pub enum GrepError {
    InvalidRegex(regex::Error),
}

impl std::fmt::Display for GrepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrepError::InvalidRegex(e) => write!(f, "invalid regex: {e}"),
        }
    }
}

impl std::error::Error for GrepError {}

/// A compiled search pattern. The regex arm uses `regex::bytes::Regex` so
/// non-UTF-8 log bytes never error.
pub enum Matcher {
    Literal { needle: Vec<u8> },
    Regex(regex::bytes::Regex),
}

pub fn compile(query: &str, is_regex: bool) -> Result<Matcher, GrepError> {
    if is_regex {
        let re = regex::bytes::Regex::new(query).map_err(GrepError::InvalidRegex)?;
        Ok(Matcher::Regex(re))
    } else {
        Ok(Matcher::Literal {
            needle: query.as_bytes().to_vec(),
        })
    }
}

/// Per-scan plan: the SIMD `memmem::Finder` for a literal needle is built
/// once per scan here — never per line. The regex arm is reused as-is.
struct Scanner<'a> {
    matcher: &'a Matcher,
    finder: Option<memmem::Finder<'a>>,
}

impl<'a> Scanner<'a> {
    fn new(matcher: &'a Matcher) -> Self {
        let finder = match matcher {
            Matcher::Literal { needle } => Some(memmem::Finder::new(needle)),
            Matcher::Regex(_) => None,
        };
        Scanner { matcher, finder }
    }

    fn make_hit(&self, file: &Arc<str>, line: &[u8], offset: u64) -> Option<GrepHit> {
        let matches = self.find_matches(line);
        if matches.is_empty() {
            return None;
        }
        let decoded = String::from_utf8_lossy(line).into_owned();
        Some(GrepHit {
            file: Arc::clone(file),
            offset,
            line: decoded,
            matches,
        })
    }

    fn find_matches(&self, line: &[u8]) -> Vec<(usize, usize)> {
        match self.matcher {
            Matcher::Literal { needle } => {
                // An empty needle matches nothing (never every position).
                if needle.is_empty() {
                    return Vec::new();
                }
                let finder = self.finder.as_ref().unwrap();
                let mut out = Vec::new();
                let mut search_from = 0;
                while let Some(m) = finder.find(&line[search_from..]) {
                    let start = search_from + m;
                    out.push((start, start + needle.len()));
                    search_from = start + needle.len();
                }
                out
            }
            Matcher::Regex(re) => re.find_iter(line).map(|m| (m.start(), m.end())).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GrepHit {
    /// Bare filename the hit belongs to.
    pub file: Arc<str>,
    /// Byte offset of the start of the line within the file.
    pub offset: u64,
    pub line: String,
    /// Byte ranges of each match within the raw (undecoded) line.
    pub matches: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Reverse,
    Forward,
}

struct GrepScan<'a> {
    file_name: &'a Arc<str>,
    scanner: &'a Scanner<'a>,
    limit: usize,
    chunk_size: usize,
    tx: &'a tokio::sync::mpsc::Sender<GrepHit>,
}

/// Scan `path` for `matcher`, emitting hits over `tx`.
///
/// Returns the number of bytes examined. On a failed send (receiver dropped =
/// client disconnected) the scan returns early with the bytes seen so far.
#[allow(dead_code)]
pub fn grep(
    path: PathBuf,
    matcher: Matcher,
    direction: Direction,
    from_offset: Option<u64>,
    limit: usize,
    chunk_size: usize,
    tx: tokio::sync::mpsc::Sender<GrepHit>,
) -> io::Result<u64> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (scanned, _) = grep_multi(
        vec![(name, path)],
        matcher,
        direction,
        from_offset,
        limit,
        chunk_size,
        tx,
    )?;
    Ok(scanned)
}

/// Scan multiple files sequentially for `matcher`, emitting hits over `tx`.
///
/// `from_offset` is applied only to the first scanned file (if specified).
/// Returns `(total_scanned_bytes, files_scanned_count)`.
/// Stops immediately if `limit` hits are emitted or client disconnects.
pub fn grep_multi(
    files: Vec<(String, PathBuf)>,
    matcher: Matcher,
    direction: Direction,
    from_offset: Option<u64>,
    limit: usize,
    chunk_size: usize,
    tx: tokio::sync::mpsc::Sender<GrepHit>,
) -> io::Result<(u64, usize)> {
    if limit == 0 || files.is_empty() {
        return Ok((0, 0));
    }
    let chunk_size = chunk_size.max(1);
    let scanner = Scanner::new(&matcher);
    let mut total_scanned = 0u64;
    let mut emitted = 0usize;
    let mut files_scanned = 0usize;
    let mut chunk_buf = vec![0u8; chunk_size];

    let mut first_file = true;
    for (name, path) in files {
        if emitted >= limit || tx.is_closed() {
            break;
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let size = match file.metadata() {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if size == 0 {
            files_scanned += 1;
            first_file = false;
            continue;
        }
        files_scanned += 1;
        let file_arc: Arc<str> = Arc::from(name);
        let scan = GrepScan {
            file_name: &file_arc,
            scanner: &scanner,
            limit,
            chunk_size,
            tx: &tx,
        };
        let offset = if first_file { from_offset } else { None };
        first_file = false;

        let scanned = match direction {
            Direction::Reverse => {
                grep_reverse(&file, size, offset, &scan, &mut emitted, &mut chunk_buf)?
            }
            Direction::Forward => {
                grep_forward(&file, size, offset, &scan, &mut emitted, &mut chunk_buf)?
            }
        };
        total_scanned += scanned;
    }

    Ok((total_scanned, files_scanned))
}

/// Emit matching lines newest-first, starting at `from_offset` (default EOF).
fn grep_reverse(
    file: &File,
    size: u64,
    from_offset: Option<u64>,
    scan: &GrepScan<'_>,
    emitted: &mut usize,
    chunk: &mut [u8],
) -> io::Result<u64> {
    let end = from_offset.unwrap_or(size).min(size);

    // If the scan range ends right after a '\n', the first (closest-to-end)
    // line is the empty trailing segment; drop it, exactly like `tail`.
    let skip_first = if end == 0 {
        false
    } else {
        let mut last = [0u8; 1];
        file.read_exact_at(&mut last, end - 1)?;
        last[0] == b'\n'
    };

    let mut pos = end;
    // `tail` holds the (earlier-in-file) prefix of the line whose newline has
    // not been found yet; `tail_over` marks that line as oversized (discarded).
    let mut tail: Vec<u8> = Vec::new();
    let mut tail_over = false;
    let mut scanned = 0u64;
    // Reused assembly buffer: no per-line allocation.
    let mut line_buf: Vec<u8> = Vec::new();
    let mut first_line = true;

    while pos > 0 {
        if scan.tx.is_closed() {
            return Ok(scanned);
        }
        let read_start = pos.saturating_sub(scan.chunk_size as u64);
        let read_len = (pos - read_start) as usize;
        file.read_exact_at(&mut chunk[..read_len], read_start)?;
        scanned += read_len as u64;
        let buf = &chunk[..read_len];

        let mut line_end = read_len;
        while let Some(i) = memrchr(b'\n', &buf[..line_end]) {
            let seg_len = line_end - i - 1;
            let over = tail_over || seg_len + tail.len() > MAX_LINE_BYTES;
            if !(first_line && skip_first) && !over {
                line_buf.clear();
                line_buf.extend_from_slice(&buf[i + 1..line_end]);
                line_buf.extend_from_slice(&tail);
                let line_start = read_start + i as u64 + 1;
                if let Some(hit) = scan.scanner.make_hit(scan.file_name, &line_buf, line_start) {
                    if scan.tx.blocking_send(hit).is_err() {
                        return Ok(scanned);
                    }
                    *emitted += 1;
                    if *emitted >= scan.limit {
                        return Ok(scanned);
                    }
                }
            }
            first_line = false;
            tail_over = false;
            tail.clear();
            line_end = i;
        }

        // Leftover prefix (no '\n' in it) continues the current line backward.
        if !tail_over {
            line_buf.clear();
            line_buf.extend_from_slice(&buf[..line_end]);
            line_buf.extend_from_slice(&tail);
            std::mem::swap(&mut line_buf, &mut tail);
            if tail.len() > MAX_LINE_BYTES {
                tail.clear();
                tail_over = true;
            }
        }
        pos = read_start;
    }

    // The first line of the file (offset 0) may be empty; emit it iff the
    // scan range was non-empty and it is not oversized.
    if end > 0
        && !tail_over
        && let Some(hit) = scan.scanner.make_hit(scan.file_name, &tail, 0)
    {
        if scan.tx.blocking_send(hit).is_err() {
            return Ok(scanned);
        }
        *emitted += 1;
    }

    Ok(scanned)
}

/// Emit matching lines oldest-first, starting at `from_offset` (default 0).
fn grep_forward(
    file: &File,
    size: u64,
    from_offset: Option<u64>,
    scan: &GrepScan<'_>,
    emitted: &mut usize,
    chunk: &mut [u8],
) -> io::Result<u64> {
    let mut pos = from_offset.unwrap_or(0).min(size);
    let mut partial: Vec<u8> = Vec::new();
    let mut partial_start = pos;
    // The line being accumulated exceeds the cap: discard until its newline.
    let mut partial_over = false;
    let mut scanned = 0u64;
    // Reused assembly buffer: no per-line allocation.
    let mut line_buf: Vec<u8> = Vec::new();

    while pos < size {
        if scan.tx.is_closed() {
            return Ok(scanned);
        }
        let want = scan.chunk_size.min((size - pos) as usize);
        let n = file.read_at(&mut chunk[..want], pos)?;
        if n == 0 {
            break;
        }
        scanned += n as u64;
        let chunk_slice = &chunk[..n];

        let mut line_start_idx = 0;
        while let Some(rel) = memchr(b'\n', &chunk_slice[line_start_idx..]) {
            let i = line_start_idx + rel;
            let seg_len = i - line_start_idx;
            if !partial_over && partial.len() + seg_len <= MAX_LINE_BYTES {
                line_buf.clear();
                line_buf.extend_from_slice(&partial);
                line_buf.extend_from_slice(&chunk_slice[line_start_idx..i]);
                if let Some(hit) = scan
                    .scanner
                    .make_hit(scan.file_name, &line_buf, partial_start)
                {
                    if scan.tx.blocking_send(hit).is_err() {
                        return Ok(scanned);
                    }
                    *emitted += 1;
                    if *emitted >= scan.limit {
                        return Ok(scanned);
                    }
                }
            }
            partial.clear();
            partial_over = false;
            line_start_idx = i + 1;
            partial_start = pos + line_start_idx as u64;
        }

        partial.extend_from_slice(&chunk_slice[line_start_idx..n]);
        if partial.len() > MAX_LINE_BYTES {
            partial.clear();
            partial_over = true;
        }
        pos += n as u64;
    }

    // Flush a trailing unterminated line.
    if !partial.is_empty()
        && !partial_over
        && let Some(hit) = scan
            .scanner
            .make_hit(scan.file_name, &partial, partial_start)
    {
        if scan.tx.blocking_send(hit).is_err() {
            return Ok(scanned);
        }
        *emitted += 1;
    }

    Ok(scanned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use tokio::sync::mpsc;

    fn write_file(path: &std::path::Path, content: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(content).unwrap();
    }

    async fn run(
        path: &std::path::Path,
        query: &str,
        is_regex: bool,
        direction: Direction,
        from_offset: Option<u64>,
        limit: usize,
        chunk: usize,
    ) -> (Vec<GrepHit>, u64) {
        let (tx, mut rx) = mpsc::channel(16);
        let matcher = compile(query, is_regex).unwrap();
        let path = path.to_path_buf();
        let handle = tokio::task::spawn_blocking(move || {
            grep(path, matcher, direction, from_offset, limit, chunk, tx)
        });
        let mut hits = Vec::new();
        while let Some(h) = rx.recv().await {
            hits.push(h);
        }
        let scanned = handle.await.unwrap().unwrap();
        (hits, scanned)
    }

    #[test]
    fn invalid_regex_errors() {
        assert!(matches!(
            compile("(", true),
            Err(GrepError::InvalidRegex(_))
        ));
        assert!(compile("(abc", false).is_ok());
    }

    #[test]
    fn empty_literal_never_matches() {
        let m = compile("", false).unwrap();
        let s = Scanner::new(&m);
        assert!(s.find_matches(b"foo").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn literal_match_forward() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"INFO ok\nERROR boom\nINFO fine\n");
        let (hits, _) = run(&p, "ERROR", false, Direction::Forward, None, 100, 8).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "ERROR boom");
        assert_eq!(hits[0].offset, 8);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn regex_match_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"abc 123 xyz\n");
        let (hits, _) = run(&p, r"\d+", true, Direction::Forward, None, 100, 64).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matches, vec![(4, 7)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reverse_yields_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(
            &p,
            b"1 ERROR first\n2 INFO\n3 ERROR second\n4 INFO\n5 ERROR third\n",
        );
        let (hits, _) = run(&p, "ERROR", false, Direction::Reverse, None, 100, 8).await;
        let lines: Vec<&str> = hits.iter().map(|h| h.line.as_str()).collect();
        assert_eq!(
            lines,
            vec!["5 ERROR third", "3 ERROR second", "1 ERROR first"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_honored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("ERROR {i}\n"));
        }
        write_file(&p, content.as_bytes());
        let (hits, _) = run(&p, "ERROR", false, Direction::Reverse, None, 5, 8).await;
        assert_eq!(hits.len(), 5);
        assert_eq!(hits[0].line, "ERROR 99");
        assert_eq!(hits[4].line, "ERROR 95");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_line_across_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        // A long matching line spanning several 16-byte chunks.
        let long = format!("pad {} pad\n", "x".repeat(100));
        write_file(&p, long.as_bytes());
        let (hits, _) = run(&p, "xxxxx", false, Direction::Forward, None, 100, 16).await;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].line.contains("xxxxx"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_receiver_stops_worker() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = String::new();
        for i in 0..100_000 {
            content.push_str(&format!("ERROR line {i}\n"));
        }
        write_file(&p, content.as_bytes());
        let (tx, rx) = mpsc::channel::<GrepHit>(4);
        let matcher = compile("ERROR", false).unwrap();
        let path = p.clone();
        let handle = tokio::task::spawn_blocking(move || {
            grep(path, matcher, Direction::Forward, None, usize::MAX, 16, tx)
        });
        drop(rx);
        // Must complete promptly, not block on a full channel.
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("worker did not stop after receiver dropped");
        res.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_lines_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        write_file(&p, b"\nfoo\n\nbar");
        let (hits, _) = run(&p, "^$", true, Direction::Forward, None, 100, 16).await;
        let offsets: Vec<u64> = hits.iter().map(|h| h.offset).collect();
        assert_eq!(offsets, vec![0, 5]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_line_is_skipped() {
        // A single line larger than MAX_LINE_BYTES (containing the needle)
        // is skipped in both directions; the normal line after it still hits.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut content = vec![b'x'; 2 * MAX_LINE_BYTES];
        let at = MAX_LINE_BYTES / 2;
        content[at..at + 5].copy_from_slice(b"ERROR");
        content.push(b'\n');
        content.extend_from_slice(b"ERROR small\n");
        write_file(&p, &content);

        let (hits, _) = run(&p, "ERROR", false, Direction::Forward, None, 100, 65536).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "ERROR small");

        let (hits, _) = run(&p, "ERROR", false, Direction::Reverse, None, 100, 65536).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "ERROR small");
    }

    async fn run_multi(
        files: Vec<(&str, std::path::PathBuf)>,
        query: &str,
        is_regex: bool,
        direction: Direction,
        limit: usize,
        chunk: usize,
    ) -> (Vec<GrepHit>, (u64, usize)) {
        let (tx, mut rx) = mpsc::channel(16);
        let matcher = compile(query, is_regex).unwrap();
        let files = files.into_iter().map(|(n, p)| (n.to_string(), p)).collect();
        let handle = tokio::task::spawn_blocking(move || {
            grep_multi(files, matcher, direction, None, limit, chunk, tx)
        });
        let mut hits = Vec::new();
        while let Some(h) = rx.recv().await {
            hits.push(h);
        }
        let res = handle.await.unwrap().unwrap();
        (hits, res)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_sequential_scan() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("app.log");
        let p2 = dir.path().join("catalina.log");
        let p3 = dir.path().join("access.log");
        write_file(&p1, b"app line 1\nERROR app error\n");
        write_file(&p2, b"catalina 1\nERROR cat error\n");
        write_file(&p3, b"access 1\n");

        let files = vec![("app.log", p1), ("catalina.log", p2), ("access.log", p3)];

        let (hits, (scanned, files_scanned)) =
            run_multi(files, "ERROR", false, Direction::Forward, 100, 64).await;
        assert_eq!(hits.len(), 2);
        assert_eq!(&*hits[0].file, "app.log");
        assert_eq!(hits[0].line, "ERROR app error");
        assert_eq!(&*hits[1].file, "catalina.log");
        assert_eq!(hits[1].line, "ERROR cat error");
        assert_eq!(files_scanned, 3);
        assert!(scanned > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_limit_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("f1.log");
        let p2 = dir.path().join("f2.log");
        let p3 = dir.path().join("f3.log");
        write_file(&p1, b"ERROR 1\nERROR 2\nERROR 3\n");
        write_file(&p2, b"ERROR 4\nERROR 5\nERROR 6\n");
        write_file(&p3, b"ERROR 7\n");

        let files = vec![("f1.log", p1), ("f2.log", p2), ("f3.log", p3)];

        let (hits, (scanned, files_scanned)) =
            run_multi(files, "ERROR", false, Direction::Forward, 4, 64).await;
        assert_eq!(hits.len(), 4);
        assert_eq!(&*hits[0].file, "f1.log");
        assert_eq!(&*hits[2].file, "f1.log");
        assert_eq!(&*hits[3].file, "f2.log");
        assert_eq!(hits[3].line, "ERROR 4");
        // f3.log should never have been opened/scanned
        assert_eq!(files_scanned, 2);
        assert!(scanned > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_empty_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("empty.log");
        let p2 = dir.path().join("app.log");
        write_file(&p1, b"");
        write_file(&p2, b"ERROR hit\n");

        let files = vec![("empty.log", p1), ("app.log", p2)];

        let (hits, (_, files_scanned)) =
            run_multi(files, "ERROR", false, Direction::Forward, 100, 64).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0].file, "app.log");
        assert_eq!(files_scanned, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_multi_client_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("f1.log");
        let p2 = dir.path().join("f2.log");
        let mut content = String::new();
        for i in 0..10_000 {
            content.push_str(&format!("ERROR {i}\n"));
        }
        write_file(&p1, content.as_bytes());
        write_file(&p2, content.as_bytes());

        let (tx, rx) = mpsc::channel::<GrepHit>(4);
        let matcher = compile("ERROR", false).unwrap();
        let files = vec![("f1.log".to_string(), p1), ("f2.log".to_string(), p2)];
        let handle = tokio::task::spawn_blocking(move || {
            grep_multi(files, matcher, Direction::Forward, None, 100_000, 16, tx)
        });
        drop(rx);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("grep_multi did not stop after receiver dropped");
        res.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_client_disconnect_with_no_matches_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("huge.log");
        // Write large content with zero matching lines
        let mut content = String::new();
        for i in 0..20_000 {
            content.push_str(&format!("INFO line {i}\n"));
        }
        write_file(&p, content.as_bytes());

        let (tx, rx) = mpsc::channel::<GrepHit>(4);
        let matcher = compile("NONEXISTENT_NEEDLE", false).unwrap();
        let handle = tokio::task::spawn_blocking(move || {
            grep(p, matcher, Direction::Forward, None, 100_000, 64, tx)
        });
        drop(rx);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("grep did not stop promptly when receiver dropped without matches");
        let scanned = res.unwrap().unwrap();
        // Should have exited on the first chunk rather than scanning the entire file
        assert!(scanned < content.len() as u64);
    }
}
