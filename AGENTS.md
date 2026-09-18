# AGENTS.md

## What this project is

`log-sidecar` — an unprivileged Rust daemon that runs alongside Apache Tomcat and
exposes tail / grep / live-stream of large log files over HTTP, with an embedded
browser UI. Hard constraints: RSS < 15 MB, never load whole files into memory,
never block Tomcat's log writers, linear-time (ReDoS-safe) regex,
directory-traversal-proof. End state: a single static `x86_64-unknown-linux-musl`
binary plus a systemd unit, developed and tested via `devenv` tasks.

## Layout

- `src/` — the Rust crate (single binary `log-sidecar`). Module map in `src/AGENTS.md`.
- `ui/` — vanilla TypeScript frontend, bundled by esbuild. See `ui/AGENTS.md`.
- `scripts/` — `gen-test-log.sh` (fixture generator), `measure-rss.sh` (Linux-only RSS proof).
- `deploy/` — `log-sidecar.service` (systemd unit with sandbox + `MemoryMax=64M`).
- `devenv.nix` — toolchain and the whole task DAG.
- `devenv.yaml` — devenv inputs (nixpkgs + `rust-overlay`, required for `languages.rust.channel`).
- `.cargo/config.toml` — musl cross-linker (`zig-cc-musl`).
- `testdata/` — gitignored log fixtures.

## Toolchain

- Rust stable plus the `x86_64-unknown-linux-musl` target; esbuild, zig, cargo-watch
  (all from nixpkgs). No npm/node_modules.
- Musl cross-link uses `zig cc -target x86_64-linux-musl` with
  `rustflags = ["-C", "link-self-contained=no"]`. Do **not** remove that flag — rustc's
  self-contained CRT collides with zig's bundled musl `crt1.o` (duplicate `_start`).

## Build / run / test

Everything goes through devenv tasks (see `devenv.nix`):

```sh
devenv tasks run logsidecar:check    # fmt + clippy + all tests (the gate)
devenv tasks run logsidecar:build    # cargo build (after ui:build)
devenv tasks run logsidecar:release  # cross-build static musl ELF (after check)
devenv tasks run ui:build            # esbuild the frontend into ui/dist/
devenv shell -- cargo test           # run a subset directly (e.g. `cargo test grep`)
```

Run the daemon:

```sh
devenv shell -- cargo run -- --root ./testdata/logs
```

Release artifact: `target/x86_64-unknown-linux-musl/release/log-sidecar`
(statically linked, stripped).

## Conventions

- Correct-first, terse Rust; `clap` for flags, `axum 0.8` for HTTP, `notify` for file events.
- Offsets are **byte offsets** (`u64`), never line numbers. They are stable only within
  one file generation — clients must treat `rotated`/`truncated` as a stream reset.
- ReDoS-safe matching: `regex::bytes::Regex` (linear) + `memchr` SIMD literal search.
- `--allow-public` is required to bind a non-loopback address; loopback is the default.
- `?token=` query auth exists only because `EventSource` cannot set headers.
- Hard memory caps (`src/limits.rs`): lines > 1 MiB are skipped by grep/stream, tail
  windows are capped at 4 MiB, `--chunk-size` is validated to 4 KiB..8 MiB — these
  guard the < 15 MB RSS budget against pathological files (e.g. a rotated `.gz`).
- `--max-searches` gates greps and `--max-streams` gates live streams (both → HTTP 429);
  `--token` must be non-empty, unreserved-URL-charset only.

## Common pitfalls

- The reverse scan in `src/grep.rs` must `tail.clear()` after emitting each line, or
  lines get cross-contaminated across chunk boundaries.
- A file's first line may be empty; treat `end > 0`, not `!tail.is_empty()`, as "a line exists".
- The UI flex/grid needs `min-height: 0` on `#main`/`#viewport`, or the virtual
  scroller grows to content height and renders every row.
