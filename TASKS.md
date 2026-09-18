# TASKS

## Milestone 0: Toolchain & Scaffold
- [x] 0.1 devenv.nix: rust (stable + musl target), esbuild, zig, cargo-watch; task DAG (ui:build → logsidecar:build/test/clippy/fmt/check/release)
- [x] 0.2 .cargo/config.toml musl linker → zig-cc-musl; scripts/gen-test-log.sh; .gitignore (target/, ui/dist/, testdata/)
- [x] 0.3 Cargo.toml dependency set compiles; musl release build produces a static ELF

## Milestone 1: Core Engine
- [x] 1.1 File abstraction & reverse seeker (unit tested with mock log files)
- [x] 1.2 Grep engine: linear regex + memchr SIMD line parser
- [x] 1.3 Inode tracker & file rotation event handler

## Milestone 2: Networking & Streaming API
- [x] 2.1 Axum setup with directory-traversal security middleware
- [x] 2.2 /api/tail and /api/grep endpoints
- [x] 2.3 /api/stream (SSE) powered by notify

## Milestone 3: Embedded UI & Virtualization
- [x] 3.1 Minimal UI: virtual scroll buffer + stream consumer
- [x] 3.2 Regex highlighting + match navigation rail
- [x] 3.3 rust-embed serving assets from the compiled binary

## Milestone 4: Hardening & Packaging
- [x] 4.1 Static build x86_64-unknown-linux-musl (zig cross-link)
- [x] 4.2 Resource verification (RSS < 15 MB under large-file grep)
- [x] 4.3 systemd unit with resource constraints

## Milestone 5: Review hardening
- [x] 5.1 `config.rs`: IPv6-safe `socket_addr()` (no panic), `--chunk-size` bounds, `--max-streams`, non-empty unreserved-charset token validation
- [x] 5.2 `limits.rs`: `MAX_LINE_BYTES` (1 MiB) / `MAX_TAIL_WINDOW_BYTES` (4 MiB) caps across grep/stream/tail
- [x] 5.3 `grep.rs`: per-scan SIMD `Finder` (not per line), reused line buffers, oversized-line skip both directions
- [x] 5.4 `http.rs`: `spawn_blocking` for tail + file list, typed sorted `FileEntry`, exact `truncated` (limit+1 probe), `direction` 400, `--max-streams` gate, zero-copy asset fallback, `Cache-Control`/`X-Accel-Buffering` on SSE, JSON 404 for `/api`
- [x] 5.5 `watcher.rs`/`rotate.rs`: hoisted buffers, notify-error logging, high-water truncation detection
- [x] 5.6 `error.rs`: `Internal` detail logged server-side, generic client message
- [x] 5.7 `Cargo.toml`: `[profile.release]` strip/LTO/codegen-units; 58 tests (21 new)
