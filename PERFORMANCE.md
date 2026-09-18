# Performance & Architectural Review Findings

This document summarizes the performance, architecture, and correctness review
conducted on the match context inspection and configurable tail limits
implementation, and records the resolution of each finding.

**Status legend**: ✅ fixed · 🔧 fixed with adjustment (see notes) · ⚠️ accepted by design.

---

## 1. Performance Gaps & Memory Budget Constraints

### 1.1 Unconditional 4 MiB Read in `read_around` Threatens the `< 15 MB` RSS Hard Constraint — ✅
* **Location**: `src/logfile.rs` (`read_around`, forward-scan step)
* **Severity**: High
* **Hard Constraint**: `AGENTS.md` mandates daemon RSS `< 15 MB` under all conditions.
* **Mechanism**:
  `read_around` allocated and read `MAX_TAIL_WINDOW_BYTES` (4 MiB) in a single
  forward pass even when the client requested `lines = 500` (typically 30–60 KiB)
  or `lines = 4`. Near-EOF backfill additionally held `prefix` (up to 4 MiB) and
  `full` (reallocated, up to 4 MiB) concurrently with `bytes` — up to ~12 MiB of
  transient raw buffers per request, on top of `String::from_utf8_lossy`,
  `Vec<String>`, and JSON serialization. Multiple concurrent requests could
  breach the 15 MB RSS budget.
* **Resolution**:
  The forward scan now proceeds in bounded `chunk_size` steps, extending a
  single `bytes` buffer only until `lines` newlines are collected, EOF is
  reached, or the window cap is hit. Peak allocation scales with the actual
  window, never `MAX_TAIL_WINDOW_BYTES`. Verified: 50 consecutive context
  requests against a 2.3 MB fixture leave daemon RSS flat (~12 MB debug build).

---

### 1.2 $O(N^2)$ DOM Thrashing in Match Rail (`renderRail`) — ✅
* **Location**: `ui/app.ts` (`renderRail`, grep `match` handler)
* **Severity**: High (UI Responsiveness)
* **Mechanism**:
  `renderRail()` wiped `railEl.innerHTML` and rebuilt every tick with its own
  inline `click` listener on every SSE `match` event. For a 1,000-match search:
  $$\sum_{k=1}^{1000} k = \frac{1000 \times 1001}{2} = 500{,}500 \text{ DOM elements and event listeners}$$
* **Resolution**:
  1. One delegated `click` listener on `railEl` resolves the clicked tick via
     `dataset.offset` (stored on the tick element).
  2. Ticks are appended incrementally (`appendRailTick`) as matches stream in;
     a full rebuild (`renderRail`) happens only on resets (file open, new
     search, clear) and when the `done` event finalizes `fileSize`.

---

### 1.3 Redundant HTTP Requests During In-Window Match Navigation — ✅
* **Location**: `ui/app.ts` (`goToMatch`)
* **Severity**: Medium
* **Mechanism**:
  In context view, F3/`n`/`N`/▲/▼ always called `showContextAround`, clearing
  the buffer, refetching `/api/tail`, and resetting scroll — even when the
  adjacent match was already rendered inside the active context window.
* **Resolution**:
  `goToMatch` binary-searches the buffer for the match's byte offset
  (`bufferLineIndexForOffset`); when present it scrolls locally
  (`scrollToLineCentered`), moves the anchor highlight, and skips the network
  round-trip. A miss (offset drift from lossy-UTF-8 or window edge) falls back
  to the refetch.

---

### 1.4 Dual Rendering in `VirtualScroller.setSourceCentered` — ✅
* **Location**: `ui/virtual-scroll.ts`
* **Severity**: Low
* **Mechanism**:
  Setting `viewport.scrollTop` dispatches a `scroll` event (asynchronously),
  whose listener calls `render()`; `setSourceCentered` also called `render()`
  directly, producing a redundant second render of ~60+ rows.
* **Resolution**:
  Programmatic `scrollTop` updates go through a private `setScrollTop` that
  arms a `suppressScrollRender` flag only when the value actually changed; the
  scroll listener consumes and clears the flag. The direct render remains for
  the no-change case (where no event fires).

---

## 2. Correctness & Edge-Case Bugs

### 2.1 Unterminated File at EOF Causes Line Count Mismatch and Over-Backfill — 🔧
* **Location**: `src/logfile.rs` (`read_around`, EOF backfill branch)
* **Severity**: High
* **Mechanism**:
  For an actively written file whose final line lacks a trailing `\n`,
  `newline_count` undercounted the real lines in `bytes` by one, so `missing`
  was overestimated by one and the near-EOF backfill pulled an extra line;
  the function returned `lines + 1` lines (e.g. `lines = 1` at EOF returned 2).
* **Resolution** (adjustment to the original remediation):
  The effective line count now accounts for the unterminated tail:
  ```rust
  let ends_with_nl = bytes.last() == Some(&b'\n');
  let effective_lines = newline_count + usize::from(!ends_with_nl && !bytes.is_empty());
  let missing = lines.saturating_sub(effective_lines);
  ```
  However, the backfill scan target **stays `missing + 1`**: `start` sits
  immediately *after* a newline, so the 1st newline found backward is that
  terminator itself; `target = missing + 1` yields exactly `missing` additional
  lines. (Changing the target to `missing` — as first attempted — silently
  disabled backfill for terminated files.) Regression tests cover both the
  unterminated-EOF case and exact terminated-EOF backfill
  (`read_around_unterminated_last_line_at_eof`,
  `read_around_eof_backfill_is_exact`).

---

### 2.2 Out-of-Bounds `anchor_line: 0` on Empty Files & Flawed Fallback — 🔧
* **Location**: `src/http.rs` (`handle_tail`), `src/logfile.rs`
* **Severity**: Medium
* **Mechanism**:
  1. `handle_tail` overrode `w.anchor_line` with `anchor_line_index(&w, …)`,
     which returned `0` for empty windows — the API then advertised
     `{"lines": [], "anchor_line": 0}`, an invalid index into an empty array.
  2. The `anchor_line_index` fallback re-derived offsets from the decoded
     `String` lines (`s.len() + 1`). **Correction to the original analysis**:
     CRLF does *not* drift this encoder — `split('\n')` keeps the `\r` in the
     line, so `s.len() + 1` equals the raw byte length. Multi-byte UTF-8 does
     not drift either (UTF-8 `String::len` is byte length). Only lossy UTF-8
     replacement (`\u{FFFD}`, 3 bytes per invalid sequence) drifts it.
* **Resolution**:
  `handle_tail` now serializes `w.anchor_line` from `read_around` directly and
  `None` when `w.lines` is empty (field omitted via
  `skip_serializing_if`). The `anchor_line_index` fallback was deleted —
  `read_around` computes the anchor from raw byte offsets, which is always
  more accurate than any re-derivation from decoded strings. Covered by
  `tail_around_offset_on_empty_file_omits_anchor`.

---

## 3. Architectural, Concurrency & State Management Gaps

### 3.1 Unclosed Grep EventSource Holds Search Worker Permit on Context Jump — ✅
* **Location**: `ui/app.ts` (`showContextAround`)
* **Severity**: High
* **Mechanism**:
  Single-clicking a match row set `grepMode = false` without closing the grep
  SSE connection. **Precision**: this is not a permanent leak — the permit is
  released when the scan completes — but during a long scan on a large file the
  `--max-searches` semaphore stays occupied, so new searches from other tabs
  get HTTP 429 until the abandoned scan finishes. Wasted server work either
  way.
* **Resolution**:
  `showContextAround` calls `closeActiveSource()` up front, releasing the
  permit immediately. Tradeoff accepted: matches that would have streamed into
  `grepBuffer` after navigation are no longer collected; "back to search"
  shows what arrived before the jump.

---

### 3.2 Follow Mode Stream Gap and Data Loss on Search Clear — ✅
* **Location**: `ui/app.ts` (`clearSearch`, `startFollow`)
* **Severity**: Medium
* **Mechanism**:
  `clearSearch()` restored the pre-search buffer and re-entered follow via
  `/api/stream` *without* `from_offset`, so the watcher started at the EOF at
  reconnect time — every line written during the search session was skipped,
  leaving a permanent gap.
* **Resolution**:
  `startFollow` accepts an optional `fromOffset`; `clearSearch` resumes from
  `bufferEndOffset()` (byte offset just past the last restored line), so the
  stream replays exactly the missed range. Explicit user follow toggles still
  start at current EOF (no offset), which is correct for a fresh follow.

---

### 3.3 Tail Limit Dropdown Desynchronization — ✅
* **Location**: `ui/app.ts` (`tailLimitEl` change handler)
* **Severity**: Low
* **Mechanism**:
  `preSearchState` cached the buffer at the old limit; changing `#tail-limit`
  during a search session and then clearing restored the stale window while the
  dropdown (and status label) showed the new limit.
* **Resolution**:
  The `tail-limit` change handler now drops `preSearchState` unconditionally;
  ✕ clear then falls through to `openFile(currentFile)`, refetching at the new
  limit. (In plain tail view the snapshot is already `null`, so this is a
  no-op there.)

---

### 3.4 Single-Click Grep Row Navigation Interferes with Text Selection — ⚠️
* **Location**: `ui/app.ts` (viewport click handler)
* **Severity**: Low (UX)
* **Mechanism**:
  Any single click on a grep row navigates to the context view.
* **Decision — accepted by design, no change**:
  The existing filters already cover the real conflict cases: clicks with
  `e.detail > 1` (double-click word selection) are ignored, and clicks that
  ended a drag with a non-empty selection are ignored. Single-click-to-context
  is the core interaction of grep mode; moving it to double-click or a
  dedicated button would slow the primary path to protect a case the
  selection check already handles.
