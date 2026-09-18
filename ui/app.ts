import { VirtualScroller, LineSource, LineData } from "./virtual-scroll";
import { highlight } from "./highlight";

// ---- token handling --------------------------------------------------------
// The token lives in sessionStorage; fetches send it as a header, SSE URLs
// append it as a query parameter (EventSource cannot set headers).

function getToken(): string | null {
  return sessionStorage.getItem("ls-token");
}

async function apiFetch(path: string, retried = false): Promise<Response> {
  const t = getToken();
  const init: RequestInit = t ? { headers: { Authorization: `Bearer ${t}` } } : {};
  const res = await fetch(path, init);
  if (res.status === 401 && !retried) {
    const entered = window.prompt("Token required:");
    if (entered === null) throw new Error("auth cancelled");
    sessionStorage.setItem("ls-token", entered);
    return apiFetch(path, true);
  }
  return res;
}

function sseUrl(path: string): string {
  const t = getToken();
  return t ? `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(t)}` : path;
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

// ---- state -----------------------------------------------------------------

let currentFile: string | null = null;
let fileSize = 0;
let query = "";
let isRegex = false;
let activeSource: EventSource | null = null;
let buffer = new LineBuffer();
let matches: { offset: number; lineIndex: number }[] = [];

const scroller = new VirtualScroller(viewportEl, buffer, (text) =>
  highlight(text, query, isRegex).html,
);

// ---- files -----------------------------------------------------------------

async function loadFiles(): Promise<void> {
  try {
    const res = await apiFetch("/api/files");
    if (!res.ok) throw new Error(`files ${res.status}`);
    const data: { files: { name: string; size: number }[] } = await res.json();
    fileListEl.innerHTML = "";
    for (const f of data.files) {
      const li = document.createElement("li");
      li.textContent = f.name;
      li.title = `${f.size} bytes`;
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
    child.classList.toggle("active", child.textContent === name);
  }
  closeActiveSource();
  followEl.checked = false;
  buffer.clear();
  matches = [];
  query = "";
  isRegex = false;
  queryEl.value = "";
  renderRail();

  const res = await apiFetch(`/api/tail?file=${encodeURIComponent(name)}&lines=500`);
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

  closeActiveSource();
  followEl.checked = false;
  buffer.clear();
  matches = [];
  scroller.setSource(buffer);
  renderRail();

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
    const lineIndex = buffer.length();
    buffer.push(d.offset, d.line);
    matches.push({ offset: d.offset, lineIndex });
    scroller.refresh();
    renderRail();
  });
  es.addEventListener("done", (ev: MessageEvent) => {
    const d = JSON.parse(ev.data);
    statusEl.textContent = `${d.matches} matches`;
    closeActiveSource();
  });
  es.addEventListener("error", () => {
    statusEl.textContent = "search error";
    closeActiveSource();
  });
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
    tick.addEventListener("click", () => scroller.scrollToLine(m.lineIndex));
    railEl.appendChild(tick);
  }
}

// ---- wiring ----------------------------------------------------------------

queryEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") startGrep();
});

followEl.addEventListener("change", () => {
  if (followEl.checked) startFollow();
  else closeActiveSource();
});

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closeActiveSource();
    statusEl.textContent = "cancelled";
  }
});

void loadFiles();
