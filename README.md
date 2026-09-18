# log-sidecar

An unprivileged Rust daemon that runs alongside Apache Tomcat and exposes
tail / grep / live-stream of large log files over HTTP, with an embedded
browser UI. Built to be safe next to a production JVM: it never loads a whole
file into memory, never blocks the log writers, and ships as a single static
binary.

## Features

- **Tail** — read the last N lines of any log with byte-accurate offsets.
- **Grep** — linear-time regex (ReDoS-safe) or SIMD literal search, streamed
  over SSE with backpressure and client-disconnect cancellation.
- **Follow** — live streaming (`tail -f`) with rotation and truncation
  detection (`copytruncate` and move-and-create).
- **Embedded UI** — virtual-scrolled viewer, regex highlighting, match-density
  rail, no external assets or build-step dependencies at runtime.
- **Hardened by default** — directory-traversal-proof path resolution,
  optional bearer token, bounded concurrent searches and streams, hard
  line/window caps so RSS stays ~5 MB regardless of file size, systemd sandbox.

## Requirements

- [devenv](https://devenv.sh) (provides Rust, esbuild, zig).
- Linux or macOS host for development. The release target is
  `x86_64-unknown-linux-musl`.

## Build

```sh
devenv tasks run logsidecar:check    # fmt + clippy + all tests
devenv tasks run logsidecar:release  # static musl ELF (also runs the gate)
```

The release artifact is
`target/x86_64-unknown-linux-musl/release/log-sidecar` — a statically linked,
stripped x86-64 ELF. Copy it to `/usr/local/bin/log-sidecar`.

## Run

```sh
devenv shell -- cargo run -- --root /var/log/tomcat
```

Then open <http://127.0.0.1:9090>.

Generate a test fixture and try it out:

```sh
mkdir -p testdata/logs
./scripts/gen-test-log.sh testdata/logs/catalina.out 100000
devenv shell -- cargo run -- --root ./testdata/logs
```

## Configuration

| Flag | Default | Description |
|---|---|---|
| `--root <PATH>` | `/var/log/tomcat` | Directory containing the log files (served non-recursively). |
| `--bind <IP>` | `127.0.0.1` | Listen address (IPv4 or IPv6). Non-loopback requires `--allow-public`. |
| `--port <PORT>` | `9090` | Listen port. |
| `--token <TOKEN>` | — | Optional bearer token. Non-empty, unreserved URL chars only (`A-Z a-z 0-9 _ . - ~`). When set, `/api/*` requires `Authorization: Bearer <token>` or `?token=<token>`. |
| `--max-searches <N>` | `2` | Maximum concurrent grep searches (further requests get `429`). |
| `--max-streams <N>` | `8` | Maximum concurrent live streams (further requests get `429`). |
| `--chunk-size <BYTES>` | `65536` | Read chunk size for tail/grep/stream (validated `4096..=8388608`). |
| `--allow-public` | off | Permit binding to `0.0.0.0`/`::` or any non-loopback address. |

## API

All `/api/*` endpoints accept bare file names only (no path separators); paths
are resolved under `--root` and symlink escapes are rejected.

### `GET /api/files`

Lists regular files in the root directory (non-recursive):

```json
{ "files": [ { "name": "catalina.out", "size": 6888895, "modified_unix": 1789679490, "inode": 6275576 } ] }
```

### `GET /api/tail?file=<name>&lines=500`

Returns the last `lines` (clamped 1..=5000) lines with byte offsets:

```json
{ "file": "catalina.out", "start_offset": 100, "end_offset": 200, "lines": ["...", "..."] }
```

### `GET /api/grep?file=<name>&query=<q>&is_regex=false&direction=reverse&from_offset=&limit=1000`

Server-Sent Events. `direction` is `reverse` (newest-first) or `forward`;
`limit` is clamped 1..=10000. Emits `match` events then a terminal `done`:

```
event: match
data: {"offset":123,"line":"...","matches":[[0,5]]}

event: done
data: {"matches":5,"scanned_bytes":6888895,"truncated":false}
```

An invalid regex or `direction` value returns `400`. `truncated` is exact:
it is `true` only when more matches exist past `limit`.

### `GET /api/stream?file=<name>&from_offset=`

Live follow over SSE. Emits `line`, `rotated`, and `truncated` events.
`from_offset` is optional; omitted, streaming starts at the current end of file.

### Errors

`4xx`/`5xx` responses are JSON: `{"error":"message"}`. `400` bad parameter or
regex, `401` unauthorized, `403` path escape, `404` not found, `429` search or
stream limit, `500` internal.

## Deployment

A systemd unit is provided in `deploy/log-sidecar.service`. It runs as a
dynamic user with a read-only view of `/var/log/tomcat`, `NoNewPrivileges`,
`ProtectSystem=strict`, and `MemoryMax=64M` (well above the ~5 MB working set).

```sh
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/log-sidecar /usr/local/bin/
sudo install -m 0644 deploy/log-sidecar.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now log-sidecar
```

If `/var/log/tomcat` is group-restricted, add the appropriate
`SupplementaryGroups=` (or a static user) — see the comment in the unit file.

## Development

- `devenv tasks run logsidecar:check` is the gate (fmt + clippy `-D warnings` +
  58 tests).
- `scripts/measure-rss.sh` documents the RSS guarantee (Linux-only; needs a
  large fixture).
- Architecture, module map, and per-package conventions live in `AGENTS.md`
  (root, `src/`, `ui/`).

## Security model

- Bare file names only; canonicalization + `starts_with(root)` block symlink
  escapes.
- Regex is `regex::bytes::Regex` (linear time) plus `memchr` SIMD literals —
  no ReDoS.
- File reads are `O_RDONLY` with no advisory locks, so Tomcat's writers are
  never blocked.
- Bounded mpsc channels provide backpressure and turn a client disconnect into
  worker cancellation.
- Hard memory caps (`src/limits.rs`): lines > 1 MiB are skipped by grep/stream,
  tail windows are capped at 4 MiB — keeps RSS bounded on a rotated `.gz` or a
  multi-GiB line with no newline.
- The bearer token is restricted to unreserved URL chars, so the
  `Authorization` header and `?token=` arms can never disagree.
