// Saved search patterns — frontend-only persistence in localStorage.
// The sidecar is stateless; patterns are per-browser, which is acceptable
// for an ops tool used from a fixed workstation.

const STORAGE_KEY = "ls-patterns";

export interface SavedPattern {
  id: string;
  name: string;
  query: string;
  isRegex: boolean;
}

export function loadPatterns(): SavedPattern[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function savePatterns(patterns: SavedPattern[]): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(patterns));
}

export function addPattern(query: string, isRegex: boolean, name?: string): SavedPattern | null {
  const patterns = loadPatterns();
  if (patterns.some((p) => p.query === query && p.isRegex === isRegex)) {
    return null;
  }
  const p: SavedPattern = {
    id: typeof crypto !== "undefined" && crypto.randomUUID
      ? crypto.randomUUID()
      : String(Date.now()),
    name: name || query.slice(0, 40),
    query,
    isRegex,
  };
  patterns.push(p);
  savePatterns(patterns);
  return p;
}

export function removePattern(id: string): void {
  savePatterns(loadPatterns().filter((p) => p.id !== id));
}

export function renamePattern(id: string, newName: string): void {
  const patterns = loadPatterns();
  const p = patterns.find((x) => x.id === id);
  if (p) {
    p.name = newName;
    savePatterns(patterns);
  }
}

export function reorderPatterns(ids: string[]): void {
  const patterns = loadPatterns();
  const map = new Map(patterns.map((p) => [p.id, p]));
  savePatterns(ids.map((id) => map.get(id)).filter((p): p is SavedPattern => p != null));
}
