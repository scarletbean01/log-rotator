# Performance & Architectural Review Findings

This document summarizes the performance, architecture, and correctness review conducted on the match context inspection and configurable tail limits implementation.

---

## 1. Performance Gaps & Memory Budget Constraints

### 1.1 Unconditional 4 MiB Read in `read_around` Threatens the `< 15 MB` RSS Hard Constraint
* **Location**: `src/logfile.rs:203-205`
* **Severity**: High
* **Hard Constraint**: `AGENTS.md` mandates daemon RSS `< 15 MB` under all conditions.
* **Mechanism**:
  When reading context around an anchor offset, `read_around` allocates and reads `MAX_TAIL_WINDOW_BYTES` (4 MiB) in a single pass:
  ```rust
  let max_forward = crate::limits::MAX_TAIL_WINDOW_BYTES.min(size.saturating_sub(start));
  let mut bytes = read_range(&file, start, start + max_forward)?;
  ```
  Even when the client requests `lines = 500` (which typically consumes 30–60 KiB in standard log files) or `lines = 4`:
  - A 4 MiB buffer is unconditionally read from disk and allocated on the heap.
  - If near-EOF backfill triggers, `prefix` (up to 4 MiB) and `full` (reallocated to up to 4 MiB) exist concurrently, resulting in 8–12 MiB of raw byte buffers alone.
  - After `String::from_utf8_lossy(&bytes)`, vector allocation for `lines: Vec<String>`, and JSON serialization in Axum, multiple concurrent requests or rapid match switching can easily breach the 15 MB RSS budget.
* **Remediation**:
  Scan forward in bounded `chunk_size` buffers (e.g. 64 KiB default) to locate the ending newline offset before allocating the window buffer, mirroring how `scan_newlines_backward` handles reverse scanning.

---

### 1.2 $O(N^2)$ DOM Thrashing in Match Rail (`renderRail`)
* **Location**: `ui/app.ts:336`, `ui/app.ts:343`, `ui/app.ts:453-470`
* **Severity**: High (UI Responsiveness)
* **Mechanism**:
  `renderRail()` completely wipes `railEl.innerHTML = ""` and creates fresh DOM elements with individual inline `click` listeners for every match in `matches`:
  ```ts
  function renderRail(): void {
    railEl.innerHTML = "";
    if (fileSize === 0 || matches.length === 0) return;
    const railHeight = railEl.clientHeight;
    for (const m of matches) {
      const tick = document.createElement("div");
      ...
      tick.addEventListener("click", () => { ... });
      railEl.appendChild(tick);
    }
  }
  ```
  Because `renderRail()` is invoked on every single SSE `match` event:
  For a search returning 1,000 matches (the default limit):
  $$\sum_{k=1}^{1000} k = \frac{1000 \times 1001}{2} = 500,500 \text{ DOM elements and event listeners}$$
  This creates massive garbage collection churn, main-thread blocking, and dropped frames while search results stream in.
* **Remediation**:
  1. Use event delegation by attaching a single click listener to `railEl` that calculates or reads `dataset.offset` from the clicked target.
  2. Batch rail rendering via `requestAnimationFrame` or append ticks incrementally rather than wiping and rebuilding the rail on each match.

---

### 1.3 Redundant HTTP Requests During In-Window Match Navigation
* **Location**: `ui/app.ts:474-486`
* **Severity**: Medium
* **Mechanism**:
  When viewing context (`grepMode == false`), navigating between matches via F3, Shift+F3, `n`, `N`, or the ▲ / ▼ buttons invokes:
  ```ts
  function goToMatch(index: number): void {
    ...
    if (grepMode) {
      scroller.scrollToLine(matches[currentMatchIndex].lineIndex);
    } else {
      void showContextAround(matches[currentMatchIndex].offset);
    }
  }
  ```
  Even if the adjacent match is only a few lines away and already rendered inside the active 500-line context window, `showContextAround` clears the buffer, fires an HTTP GET `/api/tail` round-trip, rebuilds the buffer, and resets scrolling. This introduces unnecessary network latency, server worker churn, and visual flashing.
* **Remediation**:
  Check if `matches[currentMatchIndex].offset` is already present within `buffer.get(0).offset` and `buffer.get(last).offset`. If present, update `activeAnchorLineIndex` and scroll locally using `scroller.scrollToLineCentered()`.

---

### 1.4 Dual Rendering in `VirtualScroller.setSourceCentered`
* **Location**: `ui/virtual-scroll.ts:65-72`
* **Severity**: Low
* **Mechanism**:
  In `VirtualScroller.setSourceCentered`:
  ```ts
  this.viewport.scrollTop = Math.min(Math.max(0, target), max);
  this.render();
  ```
  Modifying `this.viewport.scrollTop` asynchronously or synchronously dispatches the browser `scroll` event, which triggers the registered listener `this.viewport.addEventListener("scroll", () => this.render())`. Calling `this.render()` directly immediately after results in redundant consecutive rendering cycles.
* **Remediation**:
  Avoid calling `this.render()` if setting `scrollTop` triggers the scroll listener, or suppress the listener during programmatically induced scroll updates.

---

## 2. Correctness & Edge-Case Bugs

### 2.1 Unterminated File at EOF Causes Line Count Mismatch and Over-Backfill
* **Location**: `src/logfile.rs:207-240`
* **Severity**: High
* **Mechanism**:
  When counting forward to `lines`, `memchr(b'\n', sub)` counts newline characters. For an actively written log file where the final line lacks a trailing `\n`:
  - `newline_count` is 1 less than the actual number of lines in `bytes`.
  - When hitting EOF, `missing = lines - newline_count` calculates 1 extra line as "missing".
  - Near-EOF backfill queries `scan_newlines_backward` for `missing + 1` newlines, pulling in an unnecessary extra line before `start`.
  - Since `bytes` does not end in `\n`, `split('\n')` preserves the trailing unterminated line without popping.
  - The function returns `lines + 1` lines (e.g. requesting `lines = 1` around an EOF line returns 2 lines).
* **Remediation**:
  Account for whether `bytes` ends with `\n` when determining effective line count at EOF:
  ```rust
  let ends_with_nl = bytes.ends_with(b"\n");
  let effective_lines = newline_count + usize::from(!ends_with_nl && !bytes.is_empty());
  let missing = lines.saturating_sub(effective_lines);
  ```

---

### 2.2 Out-of-Bounds `anchor_line: 0` on Empty Files & Flawed Fallback
* **Location**: `src/http.rs:238-245`, `src/logfile.rs:293-310`
* **Severity**: Medium
* **Mechanism**:
  1. `read_around` accurately computes `anchor_line: Option<usize>` using raw byte offsets.
  2. However, `handle_tail` overrides `w.anchor_line` with `anchor_line_index(&w, anchor_offset)`.
  3. If the target file is empty (`size == 0`), `anchor_line_index` returns `0`, which `handle_tail` packages as `anchor_line: Some(0)`. The API returns `{"lines": [], "anchor_line": 0}`, advertising an invalid index into an empty array.
  4. In `anchor_line_index`, the fallback logic computes byte lengths as `s.len() as u64 + 1`. If lines contain invalid UTF-8 (replaced with 3-byte `\u{FFFD}` sequences), multi-byte characters, or Windows CRLF line endings, the offset accumulator drifts, identifying the wrong anchor line.
* **Remediation**:
  Rely directly on `w.anchor_line` from `TailWindow` and serialize `anchor_line: None` when `w.lines` is empty.

---

## 3. Architectural, Concurrency & State Management Gaps

### 3.1 Unclosed Grep EventSource Leaks Search Worker Permits on Context Jump
* **Location**: `ui/app.ts:367-375`
* **Severity**: High
* **Mechanism**:
  When a user single-clicks a match row to view context, `showContextAround()` is called, setting `grepMode = false`. However, `closeActiveSource()` is **not** called.
  - If grep was still streaming matches on a large file, the HTTP `/api/grep` connection remains active in the background.
  - Because grep searches are strictly limited by `--max-searches` via `try_acquire_owned()` in `src/http.rs`, the semaphore permit remains held.
  - New searches from other tabs or subsequent actions fail with HTTP 429 (`TooManyRequests`) until the abandoned search finishes.
* **Remediation**:
  Call `closeActiveSource()` inside `showContextAround()` to cleanly release the search permit when navigating away from search results.

---

### 3.2 Follow Mode Stream Gap and Data Loss on Search Clear
* **Location**: `ui/app.ts:508-521`
* **Severity**: Medium
* **Mechanism**:
  If `follow` was enabled prior to searching, `clearSearch()` restores `preSearchState` (snapshot from when search began) and calls `startFollow()`.
  Because `startFollow()` initiates `/api/stream?file=...` without providing `from_offset`, the backend watcher begins streaming from the current file EOF at connection time.
  Any log lines written to disk during the search session are skipped and never appended to the client buffer, creating a permanent gap in the log view.
* **Remediation**:
  When resuming follow on search clear, either supply `from_offset` pointing to the end of the restored buffer or fetch a fresh tail via `openFile(currentFile)`.

---

### 3.3 Tail Limit Dropdown Desynchronization
* **Location**: `ui/app.ts:508-525`, `ui/app.ts:573-585`
* **Severity**: Low
* **Mechanism**:
  If the user changes `#tail-limit` to 2,000 lines while viewing context, and subsequently clicks "✕ clear", `clearSearch()` restores `preSearchState.lines` (which was cached at 500 lines). The UI dropdown indicates "2 000 lines", but the active buffer and status label show 500 lines.
* **Remediation**:
  Invalidate `preSearchState` or trigger a fresh `openFile(currentFile)` if the tail limit changed during search inspection.

---

### 3.4 Single-Click Grep Row Navigation Interferes with Text Selection
* **Location**: `ui/app.ts:587-598`
* **Severity**: Low (UX)
* **Mechanism**:
  Clicking anywhere on a row in grep mode triggers `showContextAround(offset)`. While double-clicks and non-empty selections are filtered out, a single click intended to focus the window or start a text drag immediately navigates away to context view, frustrating users attempting to select or copy log text.
* **Remediation**:
  Provide an explicit context action (e.g. double-click or a dedicated context icon/button) or verify mouse drag delta before triggering context switch.
