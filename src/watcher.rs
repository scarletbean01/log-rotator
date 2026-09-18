//! Live log following: notify-woken reads with a stat-based fallback.
//!
//! One cheap inotify/fsevent watch on the *directory* (non-recursive); events
//! only wake the reader — actual data always comes from reading the file from
//! the tracked offset. A 1 s interval re-stats the file to guard against
//! missed/overflowed events; that is one `stat` per second, not a poll loop.
//!
//! Lines longer than [`crate::limits::MAX_LINE_BYTES`] are skipped (never
//! emitted, never buffered whole), so RSS stays bounded per stream.

use std::convert::Infallible;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use memchr::memchr;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::limits::MAX_LINE_BYTES;
use crate::rotate::{self, RotationEvent};

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Line { offset: u64, line: String },
    Rotated,
    Truncated { new_size: u64 },
    Error(String),
}

/// Follow `path`, emitting events over the returned receiver.
///
/// `from_offset: None` starts at the current EOF (tail -f from now); `Some(x)`
/// replays from byte offset `x`. The receiver is dropped → the task exits.
pub fn follow(
    path: PathBuf,
    from_offset: Option<u64>,
    chunk_size: usize,
) -> mpsc::Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel(100);
    tokio::spawn(async move {
        if let Err(e) = follow_inner(&path, from_offset, chunk_size, &tx).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    });
    rx
}

async fn follow_inner(
    path: &Path,
    from_offset: Option<u64>,
    chunk_size: usize,
    tx: &mpsc::Sender<StreamEvent>,
) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut file = File::open(path)?;
    let mut offset = file.metadata()?.len().min(from_offset.unwrap_or(u64::MAX));
    // High-water path length, for shrink-based truncation detection.
    let mut high_water = file.metadata()?.len();
    let mut partial: Vec<u8> = Vec::new();
    let mut partial_start = offset;
    let mut partial_over = false;
    let mut pending_reopen = false;

    // Reused buffers: drain runs on every wake (≥ 1/s), so it must not
    // allocate per call.
    let mut read_buf = vec![0u8; chunk_size];
    let mut line_buf: Vec<u8> = Vec::new();

    // Watch the directory; events merely wake us.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = event_tx.send(res);
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Emit any lines already present at/after `from_offset`.
    drain(
        &mut file,
        &mut offset,
        &mut partial,
        &mut partial_start,
        &mut partial_over,
        &mut read_buf,
        &mut line_buf,
        tx,
    )
    .await?;
    high_water = high_water.max(offset);

    loop {
        tokio::select! {
            maybe = event_rx.recv() => {
                match maybe {
                    None => break, // watcher channel closed
                    Some(Ok(_)) => {}
                    // e.g. inotify overflow: the 1 s stat fallback carries us.
                    Some(Err(e)) => tracing::warn!("watch event error: {e}"),
                }
            }
            _ = tick.tick() => {}
        }

        // Reopen after a rotation once the replacement file appears. Between a
        // move-and-create rotation's `mv` and `touch` the path may not exist.
        if pending_reopen {
            match File::open(path) {
                Ok(f) => {
                    file = f;
                    offset = 0;
                    high_water = file.metadata().map(|m| m.len()).unwrap_or(0);
                    partial.clear();
                    partial_start = 0;
                    partial_over = false;
                    pending_reopen = false;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }

        match rotate::check(&file, path, offset, high_water) {
            Ok(Some(RotationEvent::Rotated)) => {
                if tx.send(StreamEvent::Rotated).await.is_err() {
                    return Ok(());
                }
                pending_reopen = true;
                continue;
            }
            Ok(Some(RotationEvent::Truncated)) => {
                let new_size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if tx.send(StreamEvent::Truncated { new_size }).await.is_err() {
                    return Ok(());
                }
                offset = 0;
                high_water = new_size;
                partial.clear();
                partial_start = 0;
                partial_over = false;
            }
            Ok(None) => {}
            Err(_) => continue,
        }
        if let Ok(md) = path.metadata() {
            high_water = high_water.max(md.len());
        }

        drain(
            &mut file,
            &mut offset,
            &mut partial,
            &mut partial_start,
            &mut partial_over,
            &mut read_buf,
            &mut line_buf,
            tx,
        )
        .await?;
    }

    Ok(())
}

/// Read all newly available bytes and emit complete (newline-terminated) lines.
///
/// A trailing partial line is held until its terminating `\n` arrives; it is
/// never emitted half-written. Lines longer than [`MAX_LINE_BYTES`] are
/// discarded silently.
#[allow(clippy::too_many_arguments)]
async fn drain(
    file: &mut File,
    offset: &mut u64,
    partial: &mut Vec<u8>,
    partial_start: &mut u64,
    partial_over: &mut bool,
    buf: &mut [u8],
    line_buf: &mut Vec<u8>,
    tx: &mpsc::Sender<StreamEvent>,
) -> io::Result<()> {
    loop {
        let read_start = *offset;
        let n = file.read_at(buf, read_start)?;
        if n == 0 {
            break;
        }
        *offset = read_start + n as u64;
        let chunk = &buf[..n];

        let mut line_start = 0;
        while let Some(rel) = memchr(b'\n', &chunk[line_start..]) {
            let i = line_start + rel;
            let seg_len = i - line_start;
            if !*partial_over && partial.len() + seg_len <= MAX_LINE_BYTES {
                line_buf.clear();
                line_buf.extend_from_slice(partial);
                line_buf.extend_from_slice(&chunk[line_start..i]);
                let line_offset = *partial_start;
                if tx
                    .send(StreamEvent::Line {
                        offset: line_offset,
                        line: String::from_utf8_lossy(line_buf).into_owned(),
                    })
                    .await
                    .is_err()
                {
                    return Err(io::Error::other("receiver dropped"));
                }
            }
            partial.clear();
            *partial_over = false;
            line_start = i + 1;
            *partial_start = read_start + line_start as u64;
        }
        partial.extend_from_slice(&chunk[line_start..n]);
        if partial.len() > MAX_LINE_BYTES {
            partial.clear();
            *partial_over = true;
        }
    }
    Ok(())
}

/// Map a `StreamEvent` to an SSE event for the HTTP layer.
pub fn to_sse(ev: StreamEvent) -> Result<axum::response::sse::Event, Infallible> {
    use axum::response::sse::Event;
    Ok(match ev {
        StreamEvent::Line { offset, line } => Event::default()
            .event("line")
            .json_data(serde_json::json!({ "offset": offset, "line": line }))
            .expect("line event serializes"),
        StreamEvent::Rotated => Event::default().event("rotated"),
        StreamEvent::Truncated { new_size } => Event::default()
            .event("truncated")
            .json_data(serde_json::json!({ "new_size": new_size }))
            .expect("truncated event serializes"),
        StreamEvent::Error(msg) => Event::default().event("error").data(msg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    async fn recv_event(rx: &mut mpsc::Receiver<StreamEvent>) -> StreamEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for stream event")
            .expect("stream ended unexpectedly")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_follow_emits_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, "existing\n").unwrap();

        let mut rx = follow(path.clone(), None, 64);
        // Let the follow task open the file and record its EOF offset.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Append complete lines.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"one\ntwo\n").unwrap();
        }
        let e1 = recv_event(&mut rx).await;
        assert!(
            matches!(&e1, StreamEvent::Line { line, .. } if line == "one"),
            "unexpected: {e1:?}"
        );
        let e2 = recv_event(&mut rx).await;
        assert!(
            matches!(&e2, StreamEvent::Line { line, .. } if line == "two"),
            "unexpected: {e2:?}"
        );

        // Rotate: move-and-create.
        std::fs::rename(&path, dir.path().join("app.log.1")).unwrap();
        std::fs::write(&path, "").unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"fresh\n").unwrap();
        }

        let mut seen_rotated = false;
        let mut seen_fresh = false;
        for _ in 0..20 {
            let ev = recv_event(&mut rx).await;
            match ev {
                StreamEvent::Rotated => seen_rotated = true,
                StreamEvent::Line { line, .. } if line == "fresh" => seen_fresh = true,
                _ => {}
            }
            if seen_rotated && seen_fresh {
                break;
            }
        }
        assert!(seen_rotated, "missing rotated event");
        assert!(seen_fresh, "missing line from the new file");

        // Truncate in place.
        {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.set_len(0).unwrap();
        }
        let mut seen_truncated = false;
        for _ in 0..20 {
            let ev = recv_event(&mut rx).await;
            if matches!(ev, StreamEvent::Truncated { .. }) {
                seen_truncated = true;
                break;
            }
        }
        assert!(seen_truncated, "missing truncated event");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, "existing\n").unwrap();

        let mut rx = follow(path.clone(), None, 4096);
        // Let the follow task open the file and record its EOF offset.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // One oversized line (never buffered whole), then a normal line.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let mut big = vec![b'y'; 2 * MAX_LINE_BYTES];
            big[MAX_LINE_BYTES / 2..MAX_LINE_BYTES / 2 + 6].copy_from_slice(b"needle");
            big.push(b'\n');
            f.write_all(&big).unwrap();
            f.write_all(b"after\n").unwrap();
        }

        let mut seen_after = false;
        for _ in 0..5 {
            let ev = recv_event(&mut rx).await;
            if let StreamEvent::Line { line, .. } = &ev {
                assert!(
                    line.len() <= MAX_LINE_BYTES,
                    "oversized line leaked into the stream"
                );
                if line == "after" {
                    seen_after = true;
                    break;
                }
            }
        }
        assert!(seen_after, "missing line after the oversized one");
    }
}
