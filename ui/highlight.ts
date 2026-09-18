// Client-side match highlighting. Matches are recomputed on the rendered line
// only; an invalid regex renders an inline error instead of throwing.

export interface MatchRange {
  start: number;
  end: number;
}

export interface HighlightResult {
  html: string;
  matches: MatchRange[];
}

export function highlight(text: string, query: string, isRegex: boolean): HighlightResult {
  const empty: HighlightResult = { html: escapeHtml(text), matches: [] };
  if (!query) return empty;

  const matches = collectMatches(text, query, isRegex);
  if (matches === null) {
    return {
      html: `<span class="regex-error">${escapeHtml(text)}</span>`,
      matches: [],
    };
  }
  if (matches.length === 0) return empty;

  let html = "";
  let last = 0;
  for (const m of matches) {
    html += escapeHtml(text.slice(last, m.start));
    html += `<mark>${escapeHtml(text.slice(m.start, m.end))}</mark>`;
    last = m.end;
  }
  html += escapeHtml(text.slice(last));
  return { html, matches };
}

// Returns null for an invalid regex; otherwise the list of match ranges in
// UTF-16 code-unit indices.
function collectMatches(text: string, query: string, isRegex: boolean): MatchRange[] | null {
  if (isRegex) {
    let re: RegExp;
    try {
      re = new RegExp(query, "g");
    } catch {
      return null;
    }
    const out: MatchRange[] = [];
    let m: RegExpExecArray | null;
    while ((m = re.exec(text)) !== null) {
      out.push({ start: m.index, end: m.index + m[0].length });
      if (m.index === re.lastIndex) re.lastIndex++; // zero-length match guard
    }
    return out;
  }

  const out: MatchRange[] = [];
  let idx = text.indexOf(query);
  while (idx !== -1) {
    out.push({ start: idx, end: idx + query.length });
    idx = text.indexOf(query, idx + query.length);
  }
  return out;
}

// Timestamp parsing for the gutter column.
// Matches the three most common Tomcat/Java log timestamp formats:
//   2026-09-18T10:15:32(.481Z)  — ISO 8601
//   2026-09-18 10:15:32(.481)   — space-separated datetime
//   10:15:32(.481)              — bare time
const TS_RE =
  /^(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?)/;

export interface ParsedTimestamp {
  display: string;   // short label shown in gutter (HH:mm:ss)
  full: string;      // full matched text for tooltip
  date: Date | null; // parsed Date for relative calculation (null if bare time)
  msgStart: number;  // index into line where the message body starts
}

export function parseTimestamp(line: string): ParsedTimestamp | null {
  const m = TS_RE.exec(line);
  if (!m) return null;
  const raw = m[1];
  const end = m[0].length;
  // Skip any separating whitespace/brackets after the timestamp
  let msgStart = end;
  while (msgStart < line.length && (line[msgStart] === " " || line[msgStart] === "," || line[msgStart] === "[")) {
    msgStart++;
  }
  // Extract HH:mm:ss for the gutter label
  const timeMatch = /(\d{2}:\d{2}:\d{2})/.exec(raw);
  const display = timeMatch ? timeMatch[1] : raw.slice(0, 8);
  // Try to parse a full date; bare times (no date part) give null
  let date: Date | null = null;
  if (raw.length > 8) {
    const normalized = raw.replace(" ", "T");
    const d = new Date(normalized);
    if (!isNaN(d.getTime())) date = d;
  }
  return { display, full: raw, date, msgStart };
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// Log-level classification for row coloring.
const LEVEL_RE = /\b(FATAL|SEVERE|ERROR|WARN(?:ING)?|INFO|DEBUG|TRACE)\b/;

export function classifyLevel(text: string): string {
  const m = LEVEL_RE.exec(text.length > 100 ? text.slice(0, 100) : text);
  if (!m) return "";
  const w = m[1];
  if (w === "ERROR" || w === "FATAL" || w === "SEVERE") return "level-error";
  if (w === "WARN" || w === "WARNING") return "level-warn";
  if (w === "DEBUG") return "level-debug";
  if (w === "TRACE") return "level-trace";
  return "";
}
