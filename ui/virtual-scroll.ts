// Virtualized window renderer. Only ~60 DOM rows are ever live, regardless of
// how many lines the underlying source holds.

export interface LineData {
  offset: number;
  text: string;
}

export interface LineSource {
  length(): number;
  get(index: number): LineData;
}

export type RenderResult = { html: string; className?: string };
export type RowRenderer = (text: string) => string | RenderResult;

export class VirtualScroller {
  private viewport: HTMLElement;
  private sizer: HTMLElement;
  private content: HTMLElement;
  private source: LineSource;
  private renderRow: RowRenderer;
  private lineHeight = 16;
  private overscan = 20;

  constructor(viewport: HTMLElement, source: LineSource, renderRow: RowRenderer) {
    this.viewport = viewport;
    this.source = source;
    this.renderRow = renderRow;

    this.sizer = document.createElement("div");
    this.sizer.id = "sizer";
    this.content = document.createElement("div");
    this.content.id = "content";
    this.sizer.appendChild(this.content);
    viewport.innerHTML = "";
    viewport.appendChild(this.sizer);

    this.lineHeight = measureLineHeight();
    this.viewport.addEventListener("scroll", () => this.render());
  }

  setSource(source: LineSource): void {
    this.source = source;
    this.refresh();
  }

  /** Recompute total height and re-render, preserving scroll position. */
  refresh(): void {
    this.sizer.style.height = `${this.source.length() * this.lineHeight}px`;
    this.render();
  }

  scrollToLine(index: number): void {
    const max = Math.max(0, this.source.length() * this.lineHeight - this.viewport.clientHeight);
    this.viewport.scrollTop = Math.min(index * this.lineHeight, max);
  }

  scrollToTop(): void {
    this.viewport.scrollTop = 0;
  }

  scrollToBottom(): void {
    this.viewport.scrollTop = this.viewport.scrollHeight;
  }

  isPinnedToBottom(): boolean {
    const t = this.viewport;
    return t.scrollTop + t.clientHeight >= t.scrollHeight - this.lineHeight * 2;
  }

  render(): void {
    const total = this.source.length();
    if (total === 0) {
      this.content.innerHTML = "";
      this.content.style.transform = "translateY(0px)";
      return;
    }

    const scrollTop = this.viewport.scrollTop;
    const visibleCount = Math.ceil(this.viewport.clientHeight / this.lineHeight);
    const start = Math.max(0, Math.floor(scrollTop / this.lineHeight) - this.overscan);
    const end = Math.min(total, start + visibleCount + this.overscan * 2);

    const frag = document.createDocumentFragment();
    for (let i = start; i < end; i++) {
      const d = this.source.get(i);
      const row = document.createElement("div");
      row.style.height = `${this.lineHeight}px`;
      row.dataset.offset = String(d.offset);
      const result = this.renderRow(d.text);
      if (typeof result === "string") {
        row.className = "row";
        row.innerHTML = result;
      } else {
        row.className = result.className ? `row ${result.className}` : "row";
        row.innerHTML = result.html;
      }
      frag.appendChild(row);
    }

    this.content.innerHTML = "";
    this.content.appendChild(frag);
    this.content.style.transform = `translateY(${start * this.lineHeight}px)`;
  }
}

function measureLineHeight(): number {
  const probe = document.createElement("div");
  probe.className = "row";
  probe.textContent = "M";
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  document.body.appendChild(probe);
  const height = probe.offsetHeight || 16;
  probe.remove();
  return height;
}
