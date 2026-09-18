# AGENTS.md — src/ (Rust crate)

## What this package is

A single-binary Rust crate (`log-sidecar`, edition 2024) with a pure-Rust
dependency tree (no openssl, no C build scripts), so it cross-links to musl with
zig. HTTP server, tail/grep/stream engine, and path security all live here.

## Module map

| Module | Responsibility |
|---|---|
| `main.rs` | clap parse, tracing init, runtime (2 workers / `max_searches + 4` blocking threads), axum serve, SIGINT+SIGTERM shutdown. |
| `config.rs` | `Config` (clap derive) + `validate()` (loopback/`--allow-public`, chunk-size 4 KiB..8 MiB, `--max-streams` > 0, non-empty unreserved-charset token) + `socket_addr() -> Result` (IPv6-safe). |
| `error.rs` | `AppError` → HTTP JSON `{"error":…}`; `From<io::Error>` and `From<grep::GrepError>`. `Internal` detail is `tracing::error!`-ed; clients get a generic message. |
| `pathguard.rs` | `resolve_under_root` — bare names only, `canonicalize`, `starts_with(root)` + `is_file`. **Root must already be canonical** (not re-resolved per request). |
| `limits.rs` | `MAX_LINE_BYTES` (1 MiB) and `MAX_TAIL_WINDOW_BYTES` (4 MiB) — the RSS guard rails. |
| `logfile.rs` | `FileId` (dev+ino); `read_tail` — reverse-seek tail via `memrchr` backward chunk scan; window byte-capped. |
| `grep.rs` | `Matcher`, `compile`, `Scanner` (per-scan SIMD finder), `grep` (sync), `GrepHit`, `Direction`. |
| `rotate.rs` | `RotationEvent`, `check` — inode compare + size-vs-offset and size-vs-high-water compare. |
| `watcher.rs` | `StreamEvent`, `follow` (dir watch + 1 s stat fallback), `to_sse`. Buffers are hoisted and reused across wakes. |
| `http.rs` | `AppState`, `build_router`, auth middleware, `/api/*` handlers, rust-embed static assets (single fallback route, zero-copy). |

## Threading / I/O model

- tokio multi-thread: **2 workers + `max_searches + 4` blocking threads**
  (`main.rs`). The sidecar must not hog the host; all file/dir work runs on
  the blocking pool.
- `grep`, `read_tail`, and the `/api/files` directory walk are **sync**, run
  via `tokio::task::spawn_blocking`.
- Backpressure + cancellation use bounded mpsc channels: workers call
  `blocking_send`; a failed send (receiver dropped = client disconnected) returns early.
- All file reads use `FileExt::read_at`/`read_exact_at` with `O_RDONLY`, no locks —
  Tomcat's writers are never blocked. Memory is
  `O(chunk_size + channel_capacity + MAX_LINE_BYTES)`, not `O(file_size)`.
- The watcher's `drain` reads from the tracked offset on the async task (small
  reads); its buffers are allocated once in `follow_inner`, never per wake.

## Memory limits (hard)

- Lines longer than `limits::MAX_LINE_BYTES` are **skipped** by grep and the
  live stream (never assembled, never matched). This is what keeps RSS
  bounded on a rotated `.gz` or a multi-GiB line with no `\n`.
- `read_tail` windows are byte-capped at `limits::MAX_TAIL_WINDOW_BYTES`
  (`tail -c` semantics: the first line of a capped window may be partial).
- `--chunk-size` is validated to 4096..=8388608 so scan buffers stay sane.

## Key contracts

- `grep::grep` returns scanned bytes and streams `GrepHit { offset, line, matches }`,
  newest-first (`Reverse`) or oldest-first (`Forward`). HTTP layer probes one
  hit past the limit so the `done` event's `truncated` flag is exact.
- `grep::compile` maps an invalid regex to `GrepError::InvalidRegex` → HTTP 400.
  An invalid `direction` value is also a 400 (only `forward`/`reverse`).
- `logfile::read_tail` returns `TailWindow { start_offset, end_offset, lines }`.
- Search gating in `http.rs`: `Semaphore` sized by `--max-searches`,
  `try_acquire_owned` → 429. Streams are gated separately by `--max-streams`.
  Permits live inside the SSE stream adapters and are released when the
  response body drops (client disconnect) or the stream completes.
- Auth middleware wraps `/api/*` only; static assets are exempt. The token is
  restricted to unreserved URL chars at validation so the header and `?token=`
  arms always agree.
- SSE responses carry `Cache-Control: no-cache` and `X-Accel-Buffering: no`
  (proxy buffering would break live delivery).
- Unknown `/api/*` routes return JSON 404; everything else falls through to the
  embedded-asset handler.

## Build / run / test

```sh
cargo test                                # full suite (58 tests)
cargo test grep                           # or logfile | rotate | stream subsets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Conventions / pitfalls

- Byte offsets are `u64`, not line numbers.
- Reverse grep: `tail.clear()` after each emitted line (chunk-crossing contamination).
- Empty first line is a real line — emit on `end > 0`, not `!tail.is_empty()`.
- Use `regex::bytes::Regex`, not `str`, so non-UTF-8 log bytes never error.
- `memmem::Finder` borrows its needle — it is built once per scan in
  `Scanner::new`, never per line and never stored in `Matcher`.
- Oversized-line state (`partial_over`/`tail_over`) must reset exactly at the
  line's terminating newline — the next line must not inherit the flag.
- The watcher's `pending_reopen` flag handles the `mv`/`touch` gap in a
  move-and-create rotation (path momentarily missing must not kill the stream).
- copytruncate detection compares size against both the read offset and the
  high-water mark; a truncate + regrow *beyond* the high-water mark between
  checks is inherently undetectable by size alone (documented limitation).
