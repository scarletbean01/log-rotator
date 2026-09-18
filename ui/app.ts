import { VirtualScroller, LineSource, LineData } from "./virtual-scroll";
import { highlight, classifyLevel, parseTimestamp } from "./highlight";

// ---- token handling --------------------------------------------------------
// The token lives in sessionStorage; fetches send it as a header, SSE URLs
// append it as a query parameter (EventSource cannot set headers).

function getToken(): string | null {
  return sessionStorage.getItem("ls-token");
}

async function apiFetch(path: string, retried = false, signal?: AbortSignal): Promise<Response> {
  const t = getToken();
  const headers: Record<string, string> = t ? { Authorization: `Bearer ${t}` } : {};
  const init: RequestInit = {
    headers,
    ...(signal ? { signal } : {}),
  };
  const res = await fetch(path, init);
  if (res.status === 401 && !retried) {
    const entered = window.prompt("Token required:");
    if (entered === null) throw new Error("auth cancelled");
    sessionStorage.setItem("ls-token", entered);
    return apiFetch(path, true, signal);
  }
  return res;
}

function sseUrl(path: string): string {
  const t = getToken();
  return t ? `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(t)}` : path;
}

// ---- helpers ---------------------------------------------------------------

function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
  return `${(bytes / 1073741824).toFixed(1)} GB`;
}

function relativeTime(unix: number): string {
  const delta = Math.floor(Date.now() / 1000) - unix;
  if (delta < 5) return "just now";
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

function relativeTs(date: Date): string {
  const delta = Math.floor((Date.now() - date.getTime()) / 1000);
  if (delta < 5) return "just now";
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

// ---- line buffer -----------------------------------------------------------

class LineBuffer implements LineSource {
  private lines: LineData[] = [];

  length(): number {
    return this.lines.length;
  }

  get(index: number): LineData {
    return this.lines[index];
  }

  push(offset: number, text: string): void {
    this.lines.push({ offset, text });
  }

  clear(): void {
    this.lines = [];
  }

  getAll(): LineData[] {
    return [...this.lines];
  }

  setAll(lines: LineData[]): void {
    this.lines = [...lines];
  }
}

// ---- DOM -------------------------------------------------------------------

const fileListEl = document.getElementById("file-list") as HTMLUListElement;
const queryEl = document.getElementById("query") as HTMLInputElement;
const regexEl = document.getElementById("regex") as HTMLInputElement;
const directionEl = document.getElementById("direction") as HTMLSelectElement;
const followEl = document.getElementById("follow") as HTMLInputElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;
const viewportEl = document.getElementById("viewport") as HTMLElement;
const railEl = document.getElementById("rail") as HTMLElement;
const searchBtnEl = document.getElementById("search-btn") as HTMLButtonElement;
const matchNavEl = document.getElementById("match-nav") as HTMLElement;
const matchPosEl = document.getElementById("match-pos") as HTMLSpanElement;
const prevMatchEl = document.getElementById("prev-match") as HTMLButtonElement;
const nextMatchEl = document.getElementById("next-match") as HTMLButtonElement;
const clearSearchEl = document.getElementById("clear-search") as HTMLButtonElement;
const backToSearchEl = document.getElementById("back-to-search") as HTMLButtonElement;
const tailLimitEl = document.getElementById("tail-limit") as HTMLSelectElement;

// ---- state -----------------------------------------------------------------

let currentFile: string | null = null;
let fileSize = 0;
let query = "";
let isRegex = false;
let activeSource: EventSource | null = null;
let buffer = new LineBuffer();
let grepBuffer: LineBuffer | null = null;
let matches: { offset: number; lineIndex: number }[] = [];
let currentMatchIndex = -1;
let grepMode = false;
let activeAnchorOffset: number | null = null;
let activeAnchorLineIndex: number | null = null;
let activeContextAbort: AbortController | null = null;
let preSearchState: {
  file: string;
  lines: LineData[];
  fileSize: number;
  scrollTop: number;
  wasFollow: boolean;
} | null = null;

const scroller = new VirtualScroller(viewportEl, buffer, (text, offset, index) => {
  const levelClass = classifyLevel(text);
  const classes: string[] = [];
  if (levelClass) classes.push(levelClass);
  if (grepMode) classes.push("grep-result");
  if (activeAnchorLineIndex !== null && index === activeAnchorLineIndex) classes.push("row-anchor");
  const className = classes.length > 0 ? classes.join(" ") : undefined;

  const ts = parseTimestamp(text);
  let html: string;
  if (ts) {
    const msg = text.slice(ts.msgStart);
    const msgHtml = highlight(msg, query, isRegex).html;
    const tooltip = ts.date
      ? `${ts.full} · ${relativeTs(ts.date)}`
      : ts.full;
    const tsHtml = `<span class="row-ts" title="${escHtml(tooltip)}">${escHtml(ts.display)}</span>`;
    html = `${tsHtml}<span class="row-msg">${msgHtml}</span>`;
  } else {
    html = `<span class="row-msg">${highlight(text, query, isRegex).html}</span>`;
  }
  return className ? { html, className } : html;
});

/** Minimal HTML attribute escaper for tooltip strings. */
function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}

// ---- files -----------------------------------------------------------------

async function loadFiles(): Promise<void> {
  try {
    const res = await apiFetch("/api/files");
    if (!res.ok) throw new Error(`files ${res.status}`);
    const data: { files: { name: string; size: number; modified_unix: number }[] } = await res.json();
    fileListEl.innerHTML = "";
    for (const f of data.files) {
      const li = document.createElement("li");
      li.dataset.file = f.name;
      const nameEl = document.createElement("div");
      nameEl.textContent = f.name;
      li.appendChild(nameEl);
      const metaEl = document.createElement("div");
      metaEl.className = "file-meta";
      metaEl.textContent = `${humanSize(f.size)} · ${relativeTime(f.modified_unix)}`;
      li.appendChild(metaEl);
      const now = Math.floor(Date.now() / 1000);
      if (now - f.modified_unix < 60) li.classList.add("file-live");
      li.addEventListener("click", () => void openFile(f.name));
      fileListEl.appendChild(li);
    }
  } catch (e) {
    statusEl.textContent = `failed to load files: ${e}`;
  }
}

function closeActiveSource(): void {
  if (activeSource) {
    activeSource.close();
    activeSource = null;
  }
}

async function openFile(name: string): Promise<void> {
  currentFile = name;
  for (const child of Array.from(fileListEl.children)) {
    child.classList.toggle("active", (child as HTMLElement).dataset.file === name);
  }
  closeActiveSource();
  if (activeContextAbort) {
    activeContextAbort.abort();
    activeContextAbort = null;
  }
  preSearchState = null;
  activeAnchorOffset = null;
  activeAnchorLineIndex = null;
  grepBuffer = null;
  followEl.checked = false;
  buffer.clear();
  matches = [];
  query = "";
  isRegex = false;
  queryEl.value = "";
  grepMode = false;
  currentMatchIndex = -1;
  matchNavEl.style.display = "none";
  clearSearchEl.style.display = "none";
  backToSearchEl.style.display = "none";
  renderRail();

  const res = await apiFetch(`/api/tail?file=${encodeURIComponent(name)}&lines=${tailLimitEl.value}`);
  if (!res.ok) {
    statusEl.textContent = `tail failed: ${res.status}`;
    return;
  }
  const data = await res.json();
  fileSize = data.end_offset as number;

  // Reconstruct per-line byte offsets from start_offset + line lengths.
  const encoder = new TextEncoder();
  let off = data.start_offset as number;
  for (const line of data.lines as string[]) {
    buffer.push(off, line);
    off += encoder.encode(line).length + 1; // +1 for the '\n'
  }

  scroller.setSource(buffer);
  scroller.scrollToBottom();
  statusEl.textContent = `${name} — ${buffer.length()} lines`;
}

// ---- follow ----------------------------------------------------------------

function startFollow(): void {
  if (!currentFile) return;
  closeActiveSource();
  const es = new EventSource(sseUrl(`/api/stream?file=${encodeURIComponent(currentFile)}`));
  activeSource = es;
  statusEl.textContent = "following…";

  es.addEventListener("line", (ev: MessageEvent) => {
    const d = JSON.parse(ev.data);
    const wasPinned = scroller.isPinnedToBottom();
    buffer.push(d.offset, d.line);
    scroller.refresh();
    if (wasPinned) scroller.scrollToBottom();
  });
  es.addEventListener("rotated", () => {
    buffer.clear();
    scroller.refresh();
    statusEl.textContent = "rotated";
  });
  es.addEventListener("truncated", () => {
    buffer.clear();
    scroller.refresh();
    statusEl.textContent = "truncated";
  });
  es.onerror = () => {
    statusEl.textContent = "stream closed";
    closeActiveSource();
  };
}

// ---- grep ------------------------------------------------------------------

function startGrep(): void {
  if (!currentFile) return;
  const q = queryEl.value;
  if (!q) return;
  query = q;
  isRegex = regexEl.checked;
  const direction = directionEl.value;

  if (!preSearchState) {
    preSearchState = {
      file: currentFile,
      lines: buffer.getAll(),
      fileSize,
      scrollTop: viewportEl.scrollTop,
      wasFollow: followEl.checked,
    };
  }
  activeAnchorOffset = null;
  activeAnchorLineIndex = null;
  grepBuffer = null;
  if (activeContextAbort) {
    activeContextAbort.abort();
    activeContextAbort = null;
  }

  closeActiveSource();
  followEl.checked = false;
  buffer.clear();
  matches = [];
  currentMatchIndex = -1;
  grepMode = true;
  scroller.setSource(buffer);
  renderRail();

  clearSearchEl.style.display = "inline-block";
  backToSearchEl.style.display = "none";
  matchNavEl.style.display = "flex";
  matchPosEl.textContent = "…";

  const params = new URLSearchParams({
    file: currentFile,
    query: q,
    is_regex: String(isRegex),
    direction,
    limit: "1000",
  });
  const es = new EventSource(sseUrl(`/api/grep?${params.toString()}`));
  activeSource = es;
  statusEl.textContent = "searching…";

  es.addEventListener("match", (ev: MessageEvent) => {
    const d = JSON.parse(ev.data);
    if (grepMode) {
      const lineIndex = buffer.length();
      buffer.push(d.offset, d.line);
      matches.push({ offset: d.offset, lineIndex });
      scroller.refresh();
      renderRail();
      matchPosEl.textContent = `${matches.length}`;
    } else {
      if (!grepBuffer) grepBuffer = new LineBuffer();
      const lineIndex = grepBuffer.length();
      grepBuffer.push(d.offset, d.line);
      matches.push({ offset: d.offset, lineIndex });
      renderRail();
      matchPosEl.textContent = `${currentMatchIndex >= 0 ? currentMatchIndex + 1 : 1}/${matches.length}`;
    }
  });
  es.addEventListener("done", (ev: MessageEvent) => {
    const d = JSON.parse(ev.data);
    if (typeof d.scanned_bytes === "number" && d.scanned_bytes > 0) {
      fileSize = d.scanned_bytes;
      renderRail();
    }
    if (grepMode) {
      statusEl.textContent = `${d.matches} matches${d.truncated ? " (truncated)" : ""}`;
      matchPosEl.textContent = `${matches.length}`;
    }
    closeActiveSource();
  });
  es.addEventListener("error", () => {
    if (grepMode) statusEl.textContent = "search error";
    closeActiveSource();
  });
}

// ---- context around match --------------------------------------------------

async function showContextAround(anchorOffset: number): Promise<void> {
  if (!currentFile) return;

  if (grepMode) {
    grepBuffer = new LineBuffer();
    grepBuffer.setAll(buffer.getAll());
    grepMode = false;
  }

  if (activeContextAbort) {
    activeContextAbort.abort();
  }
  activeContextAbort = new AbortController();
  const currentAbort = activeContextAbort;

  followEl.checked = false;
  activeAnchorOffset = anchorOffset;
  activeAnchorLineIndex = null;

  if (grepBuffer) {
    backToSearchEl.style.display = "inline-block";
  }

  statusEl.textContent = "loading context…";
  try {
    const res = await apiFetch(
      `/api/tail?file=${encodeURIComponent(currentFile)}&lines=${tailLimitEl.value}&around_offset=${anchorOffset}`,
      false,
      currentAbort.signal,
    );
    if (!res.ok) {
      statusEl.textContent = `context load failed: ${res.status}`;
      return;
    }
    const data = await res.json();
    if (currentAbort.signal.aborted) return;

    buffer.clear();
    const encoder = new TextEncoder();
    let off = data.start_offset as number;
    for (const line of data.lines as string[]) {
      buffer.push(off, line);
      off += encoder.encode(line).length + 1;
    }

    const targetIndex = typeof data.anchor_line === "number" ? data.anchor_line : 0;
    activeAnchorLineIndex = targetIndex;
    scroller.setSourceCentered(buffer, targetIndex);

    const mIdx = matches.findIndex((m) => m.offset === anchorOffset);
    if (mIdx >= 0) {
      currentMatchIndex = mIdx;
      matchPosEl.textContent = `${currentMatchIndex + 1}/${matches.length}`;
    }

    clearSearchEl.style.display = "inline-block";
    statusEl.textContent = `${currentFile} — ${buffer.length()} lines (around match)`;
  } catch (e) {
    if ((e as Error).name === "AbortError") return;
    statusEl.textContent = `context load failed: ${e}`;
  }
}

function backToSearch(): void {
  if (!grepBuffer) return;
  if (activeContextAbort) {
    activeContextAbort.abort();
    activeContextAbort = null;
  }
  buffer.setAll(grepBuffer.getAll());
  grepMode = true;
  activeAnchorOffset = null;
  activeAnchorLineIndex = null;
  scroller.setSource(buffer);
  backToSearchEl.style.display = "none";
  if (currentMatchIndex >= 0 && currentMatchIndex < matches.length) {
    scroller.scrollToLine(matches[currentMatchIndex].lineIndex);
    matchPosEl.textContent = `${currentMatchIndex + 1}/${matches.length}`;
  } else {
    matchPosEl.textContent = `${matches.length}`;
  }
  statusEl.textContent = `${matches.length} matches`;
}

// ---- match rail ------------------------------------------------------------

function renderRail(): void {
  railEl.innerHTML = "";
  if (fileSize === 0 || matches.length === 0) return;
  const railHeight = railEl.clientHeight;
  for (const m of matches) {
    const tick = document.createElement("div");
    tick.className = "tick";
    tick.style.top = `${Math.min(1, m.offset / fileSize) * railHeight}px`;
    tick.addEventListener("click", () => {
      if (grepMode) {
        scroller.scrollToLine(m.lineIndex);
      } else {
        void showContextAround(m.offset);
      }
    });
    railEl.appendChild(tick);
  }
}

// ---- match navigation ------------------------------------------------------

function goToMatch(index: number): void {
  if (matches.length === 0) return;
  currentMatchIndex = ((index % matches.length) + matches.length) % matches.length;
  if (grepMode) {
    scroller.scrollToLine(matches[currentMatchIndex].lineIndex);
  } else {
    void showContextAround(matches[currentMatchIndex].offset);
  }
  matchPosEl.textContent = `${currentMatchIndex + 1}/${matches.length}`;
}

function nextMatch(): void { goToMatch(currentMatchIndex + 1); }
function prevMatch(): void { goToMatch(currentMatchIndex - 1); }

function clearSearch(): void {
  closeActiveSource();
  if (activeContextAbort) {
    activeContextAbort.abort();
    activeContextAbort = null;
  }
  activeAnchorOffset = null;
  activeAnchorLineIndex = null;
  grepMode = false;
  grepBuffer = null;
  query = "";
  isRegex = false;
  queryEl.value = "";
  matches = [];
  currentMatchIndex = -1;
  clearSearchEl.style.display = "none";
  backToSearchEl.style.display = "none";
  matchNavEl.style.display = "none";
  renderRail();

  if (preSearchState && preSearchState.file === currentFile) {
    const saved = preSearchState;
    preSearchState = null;
    buffer.setAll(saved.lines);
    fileSize = saved.fileSize;
    scroller.setSource(buffer);
    viewportEl.scrollTop = saved.scrollTop;
    scroller.render();
    statusEl.textContent = `${currentFile} — ${buffer.length()} lines`;
    if (saved.wasFollow) {
      followEl.checked = true;
      startFollow();
    }
  } else if (currentFile) {
    preSearchState = null;
    void openFile(currentFile);
  }
}

searchBtnEl.addEventListener("click", startGrep);
clearSearchEl.addEventListener("click", clearSearch);
backToSearchEl.addEventListener("click", backToSearch);
prevMatchEl.addEventListener("click", prevMatch);
nextMatchEl.addEventListener("click", nextMatch);

// ---- search-as-you-type (tail view only) -----------------------------------
// Debounce at 120 ms; re-renders the already-loaded buffer with live highlights.
// Does not fire a server search — that still requires Enter / the 🔍 button.

let liveHighlightTimer = 0;

queryEl.addEventListener("input", () => {
  if (grepMode) return; // don't interfere with grep result view
  clearTimeout(liveHighlightTimer);
  liveHighlightTimer = window.setTimeout(() => {
    query = queryEl.value;
    isRegex = regexEl.checked;
    scroller.refresh();
  }, 120);
});

queryEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    clearTimeout(liveHighlightTimer); // server search wins
    startGrep();
  }
});

followEl.addEventListener("change", () => {
  if (followEl.checked) {
    if (activeAnchorOffset !== null && currentFile) {
      activeAnchorOffset = null;
      activeAnchorLineIndex = null;
      void openFile(currentFile).then(() => {
        followEl.checked = true;
        startFollow();
      });
      return;
    }
    startFollow();
  } else {
    closeActiveSource();
  }
});

tailLimitEl.addEventListener("change", () => {
  if (activeAnchorOffset !== null && currentFile) {
    void showContextAround(activeAnchorOffset);
  } else if (currentFile && !grepMode) {
    const wasFollow = followEl.checked;
    void openFile(currentFile).then(() => {
      if (wasFollow) {
        followEl.checked = true;
        startFollow();
      }
    });
  }
});

viewportEl.addEventListener("click", (e) => {
  if (!grepMode) return;
  if (e.detail > 1) return; // ignore double click text selection
  const selection = window.getSelection();
  if (selection && selection.toString().trim().length > 0) return;

  const row = (e.target as HTMLElement).closest(".row") as HTMLElement | null;
  if (!row || !row.dataset.offset) return;
  const offset = Number(row.dataset.offset);
  void showContextAround(offset);
});

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closeActiveSource();
    statusEl.textContent = "cancelled";
    return;
  }
  // Ctrl+F / Cmd+F → focus search box (prevent browser find)
  if ((e.ctrlKey || e.metaKey) && e.key === "f") {
    e.preventDefault();
    queryEl.focus();
    queryEl.select();
    return;
  }
  // Don't handle match-nav keys when typing in an input
  if (document.activeElement === queryEl) return;
  if (e.key === "F3") {
    e.preventDefault();
    if (e.shiftKey) prevMatch(); else nextMatch();
  } else if (e.key === "n" && !e.ctrlKey && !e.metaKey && !e.altKey) {
    e.preventDefault();
    nextMatch();
  } else if (e.key === "N" && !e.ctrlKey && !e.metaKey && !e.altKey) {
    e.preventDefault();
    prevMatch();
  }
});

void loadFiles();
