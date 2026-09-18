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
