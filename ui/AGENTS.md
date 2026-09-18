# AGENTS.md — ui/

## What this package is

Vanilla TypeScript frontend: no framework, no npm dependencies. Bundled by esbuild
(from nixpkgs) into `ui/dist/`, which rust-embed serves from the binary in release
builds and reads from disk in debug builds (`#[folder = "ui/dist/"]` in `src/http.rs`).

## Files

| File | Responsibility |
|---|---|
| `app.ts` | entry — file list, tail view, follow (`EventSource /api/stream`), grep (`EventSource /api/grep`), token handling, Esc cancel. |
| `virtual-scroll.ts` | `VirtualScroller` windowing: measure line height once, render `[start, end)` with overscan, `translateY` positioning. |
| `highlight.ts` | `highlight(text, query, isRegex)` → HTML with `<mark>`; invalid regex → inline error, never throws. |
| `index.html` | layout skeleton: sidebar, toolbar, viewport, match rail. |
| `style.css` | dark theme; layout grid + flex. |

## Build

```sh
devenv tasks run ui:build
# = esbuild ui/app.ts --bundle --minify --target=es2020 --outfile=ui/dist/app.js
#   + cp ui/index.html ui/style.css ui/dist/
```

The `ui:build` task uses `execIfModified = [ "ui/*.ts" "ui/*.html" "ui/*.css" ]`.
Never introduce npm/node_modules — the toolchain is esbuild only.

## Conventions

- Data source contract: `LineSource { length(): number; get(i): LineData }` where
  `LineData = { offset: number; text: string }`.
- DOM rows are capped near ~60 via overscan; each `.row` is fixed-height.
- Token: `sessionStorage["ls-token"]`; fetches send `Authorization: Bearer`,
  SSE URLs append `?token=` (EventSource cannot set headers).
- Highlighting re-matches the query on rendered lines only; the server's `matches`
  byte ranges are not used for highlight (JS operates in UTF-16 code units).

## Pitfalls

- `#main`/`#viewport` need `min-height: 0` (flex/grid), or the scroller grows to
  full content height and renders every row.
- `EventSource` custom events arrive as `MessageEvent`; parse `ev.data` as JSON.
- The tail API does not return per-line offsets; `app.ts` reconstructs byte offsets
  from `start_offset` + `TextEncoder` line lengths.
