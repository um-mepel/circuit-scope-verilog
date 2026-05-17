// TEMPORARY (v0.3.0): Breakpoint gutter + red markers removed from the
// editor extension. The state machinery, marker class, and CSS are still
// defined below but unwired. Search for "BREAKPOINTS DISABLED FOR 0.3.0"
// for the call site to uncomment.
//
import { StateEffect, StateField, RangeSetBuilder } from "@codemirror/state";
import type { Extension } from "@codemirror/state";
import {
  Decoration,
  DecorationSet,
  EditorView,
  GutterMarker,
  ViewPlugin,
  gutter,
  hoverTooltip,
} from "@codemirror/view";
import type { ActiveSpan, Breakpoint, DriverHighlights } from "../../state/debuggerStore";
import { useDebuggerStore } from "../../state/debuggerStore";

/**
 * CodeMirror extension that reacts to `useDebuggerStore` changes:
 *
 * - Paints a subtle background highlight on byte ranges currently "active"
 *   (firing at the pinned simulator time).
 * - Adds a small gutter dot on every line with at least one active span.
 * - Registers a `hoverTooltip` provider that calls `sim_eval` for the
 *   word under the cursor and shows {decimal, hex, binary, width}.
 *
 * Data flows one-way: store → effect → `StateField` → decoration set.
 * Spans are filtered by `filePath` so the editor only paints spans that
 * belong to *this* document.
 */

/** Effect dispatched when the store's active spans for this file change. */
export const setActiveSpansEffect = StateEffect.define<ActiveSpan[]>();
/** Effect dispatched when the store's "Jump to Driver" highlights change. */
export const setDriverHighlightsEffect = StateEffect.define<DriverHighlights | null>();
/** Effect dispatched when the store's breakpoints for this file change. */
export const setBreakpointsEffect = StateEffect.define<Breakpoint[]>();

/** Configuration injected at extension construction time. */
export interface DebuggerExtensionConfig {
  /** Absolute path of the file currently open in this editor, or null. */
  filePath: string | null;
}

const activeSpanDeco = Decoration.mark({
  class: "cm-active-span",
  inclusive: false,
});
const driverStmtDeco = Decoration.mark({
  class: "cm-driver-stmt",
  inclusive: false,
});
const driverBranchDeco = Decoration.mark({
  class: "cm-driver-branch",
  inclusive: false,
});
const driverCondDeco = Decoration.mark({
  class: "cm-driver-cond",
  inclusive: false,
});

function buildDecorations(spans: ActiveSpan[], docLen: number): DecorationSet {
  if (spans.length === 0) return Decoration.none;
  const sorted = [...spans]
    .filter((s) => s.start < s.end && s.start <= docLen)
    .sort((a, b) => a.start - b.start || a.end - b.end);
  const b = new RangeSetBuilder<Decoration>();
  for (const s of sorted) {
    const from = Math.max(0, Math.min(docLen, s.start));
    const to = Math.max(from + 1, Math.min(docLen, s.end));
    b.add(from, to, activeSpanDeco);
  }
  return b.finish();
}

function activeSpanField(config: DebuggerExtensionConfig) {
  return StateField.define<{
    spans: ActiveSpan[];
    decos: DecorationSet;
  }>({
    create() {
      return { spans: [], decos: Decoration.none };
    },
    update(value, tr) {
      let decos = value.decos.map(tr.changes);
      let spans = value.spans;
      for (const e of tr.effects) {
        if (e.is(setActiveSpansEffect)) {
          spans = e.value.filter((s) => s.path === config.filePath);
          decos = buildDecorations(spans, tr.newDoc.length);
        }
      }
      if (tr.docChanged) {
        decos = buildDecorations(spans, tr.newDoc.length);
      }
      return { spans, decos };
    },
    provide: (f) => EditorView.decorations.from(f, (v) => v.decos),
  });
}

/**
 * Build decorations for a single active "Jump to Driver" target:
 *   - whole statement gets a subtle highlight,
 *   - chosen branch gets a strong highlight,
 *   - condition gets a dashed underline.
 *
 * Decorations must be sorted by start position for CodeMirror's
 * RangeSetBuilder; we add in (stmt, cond, branch) order relying on stmt
 * covering everything else — the branch/cond decorations layer on top via
 * CSS rather than being nested Decoration ranges.
 */
function buildDriverDecorations(
  marks: DriverHighlights | null,
  filePath: string | null,
  docLen: number,
): DecorationSet {
  if (!marks || marks.path !== filePath) return Decoration.none;
  const clamp = (v: number) => Math.max(0, Math.min(docLen, v));
  type Mark = { from: number; to: number; deco: Decoration };
  const out: Mark[] = [];
  const stmtFrom = clamp(marks.stmtStart);
  const stmtTo = clamp(marks.stmtEnd);
  if (stmtTo > stmtFrom) out.push({ from: stmtFrom, to: stmtTo, deco: driverStmtDeco });
  if (marks.condStart != null && marks.condEnd != null) {
    const a = clamp(marks.condStart);
    const b = clamp(marks.condEnd);
    if (b > a) out.push({ from: a, to: b, deco: driverCondDeco });
  }
  if (marks.branchStart != null && marks.branchEnd != null) {
    const a = clamp(marks.branchStart);
    const b = clamp(marks.branchEnd);
    if (b > a) out.push({ from: a, to: b, deco: driverBranchDeco });
  }
  out.sort((x, y) => x.from - y.from || x.to - y.to);
  const b = new RangeSetBuilder<Decoration>();
  for (const m of out) b.add(m.from, m.to, m.deco);
  return b.finish();
}

function driverHighlightsField(config: DebuggerExtensionConfig) {
  return StateField.define<{ marks: DriverHighlights | null; decos: DecorationSet }>({
    create() {
      const initial = useDebuggerStore.getState().driverHighlights;
      const marks = initial && initial.path === config.filePath ? initial : null;
      const docLen = 0;
      const decos = buildDriverDecorations(marks, config.filePath, docLen);
      return { marks, decos };
    },
    update(value, tr) {
      let marks = value.marks;
      let decos = value.decos.map(tr.changes);
      for (const e of tr.effects) {
        if (e.is(setDriverHighlightsEffect)) {
          marks = e.value;
          decos = buildDriverDecorations(marks, config.filePath, tr.newDoc.length);
        }
      }
      if (tr.docChanged) {
        decos = buildDriverDecorations(marks, config.filePath, tr.newDoc.length);
      }
      return { marks, decos };
    },
    provide: (f) => EditorView.decorations.from(f, (v) => v.decos),
  });
}

/**
 * Per-file breakpoint snapshot mirrored into the editor state. The
 * store is the source of truth (`useDebuggerStore.breakpoints`); this
 * field exists so the gutter + line decorations can consult a
 * synchronous, file-filtered view during render.
 */
function breakpointsField(config: DebuggerExtensionConfig) {
  return StateField.define<Breakpoint[]>({
    create() {
      const all = useDebuggerStore.getState().breakpoints;
      return Object.values(all).filter((bp) => bp.path === config.filePath);
    },
    update(value, tr) {
      for (const e of tr.effects) {
        if (e.is(setBreakpointsEffect)) {
          return e.value.filter((bp) => bp.path === config.filePath);
        }
      }
      return value;
    },
  });
}

/** Keep breakpoint gutter in sync with the zustand map for this file (1:1 with applyBreakpoints). */
function syncFileBreakpointsToView(view: EditorView, filePath: string) {
  if (!filePath) return;
  const list = Object.values(useDebuggerStore.getState().breakpoints).filter(
    (b) => b.path === filePath,
  );
  view.dispatch({ effects: setBreakpointsEffect.of(list) });
}

class BreakpointMarker extends GutterMarker {
  constructor(private readonly hasCondition: boolean) {
    super();
  }
  toDOM() {
    const dot = document.createElement("span");
    dot.className = this.hasCondition
      ? "cm-breakpoint-marker cm-breakpoint-marker-cond"
      : "cm-breakpoint-marker";
    dot.title = this.hasCondition
      ? "Conditional breakpoint (right-click to edit)"
      : "Breakpoint (right-click to add condition)";
    return dot;
  }
  eq(other: GutterMarker): boolean {
    return other instanceof BreakpointMarker && other.hasCondition === this.hasCondition;
  }
}

/**
 * Gutter that turns line-level clicks into breakpoint toggles and
 * right-clicks on an existing breakpoint into a condition-edit prompt.
 * A plain `window.prompt` is used as the v1 UX per the plan ("set/edit
 * condition"). When the user enters an empty string the condition is
 * cleared; pressing Cancel (prompt returning `null`) is a no-op.
 */
function breakpointsGutter(field: ReturnType<typeof breakpointsField>, config: DebuggerExtensionConfig) {
  return gutter({
    class: "cm-breakpoint-gutter",
    lineMarker(view, line) {
      const bps = view.state.field(field, false);
      if (!bps || bps.length === 0) return null;
      // Map 1-indexed store lines to the CodeMirror line at `line.from`.
      const cmLine = view.state.doc.lineAt(line.from).number;
      for (const bp of bps) {
        if (bp.line === cmLine) {
          return new BreakpointMarker(!!(bp.condition && bp.condition.length > 0));
        }
      }
      return null;
    },
    initialSpacer: () => new BreakpointMarker(false),
    domEventHandlers: {
      mousedown(view, line, event) {
        const mouse = event as MouseEvent;
        const cmLine = view.state.doc.lineAt(line.from).number;
        if (!config.filePath) return false;
        const path = config.filePath;
        const store = useDebuggerStore.getState();
        const existing = Object.values(store.breakpoints).find(
          (bp) => bp.path === path && bp.line === cmLine,
        );
        if (mouse.button === 2 || (mouse.button === 0 && (mouse.shiftKey || mouse.altKey))) {
          // Right-click / shift-click: edit the condition via a prompt.
          mouse.preventDefault();
          const current = existing?.condition ?? "";
          const next = window.prompt(
            `Condition for breakpoint at line ${cmLine} (leave blank to clear):\n` +
              `Examples:\n  signal\n  signal == 1\n  signal != 0x0F`,
            current,
          );
          if (next == null) return true;
          store.setBreakpointCondition(path, cmLine, next);
          syncFileBreakpointsToView(view, path);
          return true;
        }
        if (mouse.button === 0) {
          mouse.preventDefault();
          store.toggleBreakpoint(path, cmLine);
          syncFileBreakpointsToView(view, path);
          return true;
        }
        return false;
      },
      contextmenu(_view, _line, event) {
        // Prevent the browser's context menu so our right-click handling
        // above owns the interaction.
        (event as MouseEvent).preventDefault();
        return true;
      },
    },
  });
}

class ActiveLineGutterMarker extends GutterMarker {
  constructor(private readonly colorClass: string) {
    super();
  }
  toDOM() {
    const dot = document.createElement("span");
    dot.className = `cm-active-gutter-dot ${this.colorClass}`;
    return dot;
  }
  eq(other: GutterMarker): boolean {
    return (
      other instanceof ActiveLineGutterMarker &&
      other.colorClass === this.colorClass
    );
  }
}

const marker = new ActiveLineGutterMarker("cm-active-gutter-dot-default");

function activeLinesGutter(field: ReturnType<typeof activeSpanField>) {
  return gutter({
    class: "cm-active-gutter",
    lineMarker(view, line) {
      const state = view.state.field(field, false);
      if (!state || state.spans.length === 0) return null;
      const from = line.from;
      const to = line.to;
      for (const s of state.spans) {
        if (s.start <= to && s.end >= from) {
          return marker;
        }
      }
      return null;
    },
    initialSpacer: () => marker,
  });
}

/**
 * `ViewPlugin` that listens to the Zustand store and pushes
 * `setActiveSpansEffect` into the editor whenever the store updates.
 */
function storeBridge(config: DebuggerExtensionConfig): Extension {
  return ViewPlugin.define((view) => {
    let lastSpans: ActiveSpan[] = [];
    let lastDriver: DriverHighlights | null = null;
    /** Fingerprint of breakpoints *for this file* — the global map can keep the same ref across unrelated mutations. */
    let lastBreakpointFp = "";
    const applyBreakpoints = (map: Record<string, Breakpoint>) => {
      const list = Object.values(map).filter((bp) => bp.path === config.filePath);
      const fp = list
        .map((b) => `${b.id}:${b.line}:${b.condition ?? ""}`)
        .sort()
        .join("|");
      if (fp === lastBreakpointFp) return;
      lastBreakpointFp = fp;
      view.dispatch({ effects: setBreakpointsEffect.of(list) });
    };
    const applySpans = (spans: ActiveSpan[]) => {
      if (spans === lastSpans) return;
      lastSpans = spans;
      view.dispatch({ effects: setActiveSpansEffect.of(spans) });
    };
    const applyDriver = (marks: DriverHighlights | null) => {
      if (marks === lastDriver) return;
      const scrolling = marks && marks.path === config.filePath && marks !== lastDriver;
      lastDriver = marks;
      view.dispatch({ effects: setDriverHighlightsEffect.of(marks) });
      if (scrolling && marks) {
        // Defer scroll until after the view has measured the new decoration
        // layout (especially important on the editor's first mount after a
        // Jump-to-Driver, when CodeMirror's viewport is still zero-height).
        const scrollToTarget = () => {
          const docLen = view.state.doc.length;
          const target = Math.max(0, Math.min(docLen, marks.stmtStart));
          view.dispatch({
            effects: EditorView.scrollIntoView(target, { y: "center" }),
            selection: { anchor: target },
          });
        };
        if (typeof requestAnimationFrame === "function") {
          requestAnimationFrame(() => {
            requestAnimationFrame(scrollToTarget);
          });
        } else {
          scrollToTarget();
        }
      }
    };
    const initial = useDebuggerStore.getState();
    applySpans(initial.activeSpans);
    applyDriver(initial.driverHighlights);
    applyBreakpoints(initial.breakpoints);
    const unsub = useDebuggerStore.subscribe((state) => {
      applySpans(state.activeSpans);
      applyDriver(state.driverHighlights);
      applyBreakpoints(state.breakpoints);
    });
    return {
      destroy() {
        unsub();
      },
    };
  });
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

/**
 * Human-friendly femtosecond clock formatter for the hover tooltip.
 * Picks the largest unit that keeps the integer portion short, e.g.
 *   1_500_000 fs  ->  "1.5 ns"
 *   42          ->  "42 fs"
 *   1_000_000_000 -> "1 us"
 */
function formatTimeFs(fs: number): string {
  if (!Number.isFinite(fs)) return "?";
  const abs = Math.abs(fs);
  const pick = (scale: number, unit: string) => {
    const v = fs / scale;
    const s = Math.abs(v) >= 100 || v === Math.trunc(v) ? v.toFixed(0) : v.toFixed(2).replace(/\.?0+$/, "");
    return `${s} ${unit}`;
  };
  if (abs >= 1e15) return pick(1e15, "s");
  if (abs >= 1e12) return pick(1e12, "ms");
  if (abs >= 1e9) return pick(1e9, "us");
  if (abs >= 1e6) return pick(1e6, "ns");
  if (abs >= 1e3) return pick(1e3, "ps");
  return `${fs} fs`;
}

function buildHoverTooltip(config: DebuggerExtensionConfig) {
  return hoverTooltip(async (view, pos) => {
    const word = view.state.wordAt(pos);
    if (!word) return null;
    const identifier = view.state.sliceDoc(word.from, word.to);
    if (!identifier || /^\d/.test(identifier)) return null;
    const res = await useDebuggerStore.getState().evalIdentifier(
      identifier,
      config.filePath,
      word.from,
    );
    if (!res) return null;
    // Snapshot the pinned time *now* (after the eval resolves) so the
    // tooltip displays exactly the clock `sim_eval` was evaluated at,
    // even if the user moves the waveform cursor while reading it.
    const evalTimeFs = useDebuggerStore.getState().pinnedTimeFs;
    return {
      pos: word.from,
      end: word.to,
      above: true,
      create() {
        const dom = document.createElement("div");
        dom.className = "cm-sim-eval-tooltip";
        const badge = res.transitioning
          ? `<span class="cm-sim-eval-badge cm-sim-eval-badge-live">transitioning</span>`
          : `<span class="cm-sim-eval-badge cm-sim-eval-badge-stable">stable</span>`;
        const timeLabel =
          evalTimeFs != null
            ? `<span class="cm-sim-eval-time">@ ${escapeHtml(formatTimeFs(evalTimeFs))}</span>`
            : "";
        const row = (label: string, curr: string, prev: string | null) => {
          const prevCell =
            res.transitioning && prev != null && prev !== curr
              ? `<code class="cm-sim-eval-prev">${escapeHtml(prev)}</code>
                 <span class="cm-sim-eval-arrow">→</span>`
              : "";
          return `<div class="cm-sim-eval-row">
            <span>${label}</span>
            <span class="cm-sim-eval-values">${prevCell}<code>${escapeHtml(curr)}</code></span>
          </div>`;
        };
        dom.innerHTML = `
          <div class="cm-sim-eval-title">
            <span class="cm-sim-eval-name">${escapeHtml(res.resolvedName)}</span>
            <span class="cm-sim-eval-width">[${res.width} bit${res.width === 1 ? "" : "s"}]</span>
          </div>
          ${row("dec", res.decimal, res.prevDecimal)}
          ${row("hex", res.hex, res.prevHex)}
          ${row("bin", res.binary, res.prevBinary)}
          <div class="cm-sim-eval-footer">${timeLabel}${badge}</div>
        `;
        return { dom };
      },
    };
  }, { hideOnChange: true });
}

const debuggerBaseTheme = EditorView.baseTheme({
  ".cm-active-span": {
    background: "rgba(255, 213, 89, 0.18)",
    borderBottom: "1px dashed rgba(255, 213, 89, 0.55)",
  },
  ".cm-driver-stmt": {
    background: "rgba(129, 140, 248, 0.22)",
    boxShadow: "inset 0 0 0 1px rgba(129, 140, 248, 0.45)",
    borderRadius: "3px",
  },
  ".cm-driver-branch": {
    background: "rgba(250, 204, 21, 0.45)",
    boxShadow: "inset 0 0 0 1px rgba(250, 204, 21, 0.95)",
    color: "#0b1220",
    fontWeight: "600",
    borderRadius: "3px",
  },
  ".cm-driver-cond": {
    background: "rgba(148, 163, 184, 0.18)",
    borderBottom: "1px dashed rgba(203, 213, 225, 0.9)",
    borderRadius: "2px",
  },
  ".cm-active-gutter-dot": {
    display: "inline-block",
    width: "6px",
    height: "6px",
    borderRadius: "50%",
    marginTop: "6px",
    marginLeft: "4px",
    background: "#facc15",
    boxShadow: "0 0 4px rgba(250, 204, 21, 0.6)",
  },
  ".cm-breakpoint-gutter": {
    // Give the breakpoint gutter a comfortable click target and a
    // pointer cursor so users know the column is interactive.
    cursor: "pointer",
    width: "16px",
    minWidth: "16px",
  },
  ".cm-breakpoint-gutter .cm-gutterElement": {
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    padding: "0",
  },
  ".cm-breakpoint-marker": {
    display: "inline-block",
    width: "10px",
    height: "10px",
    borderRadius: "50%",
    background: "#ef4444",
    boxShadow: "0 0 6px rgba(239, 68, 68, 0.75)",
  },
  ".cm-breakpoint-marker-cond": {
    // Conditional breakpoints get a ringed look to differentiate from
    // unconditional ones without adding an extra column.
    background:
      "radial-gradient(circle at center, #ef4444 0%, #ef4444 45%, #0b1220 55%, #ef4444 65%)",
    boxShadow: "0 0 8px rgba(239, 68, 68, 0.9)",
  },
  ".cm-sim-eval-tooltip": {
    padding: "6px 8px",
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: "11px",
    background: "#1f2937",
    color: "#f8fafc",
    border: "1px solid rgba(148, 163, 184, 0.3)",
    borderRadius: "4px",
    minWidth: "160px",
    lineHeight: 1.4,
  },
  ".cm-sim-eval-title": {
    fontWeight: "600",
    borderBottom: "1px solid rgba(148, 163, 184, 0.25)",
    paddingBottom: "2px",
    marginBottom: "4px",
    display: "flex",
    justifyContent: "space-between",
    gap: "8px",
  },
  ".cm-sim-eval-width": {
    color: "#94a3b8",
    fontWeight: "400",
  },
  ".cm-sim-eval-row": {
    display: "flex",
    gap: "10px",
    justifyContent: "space-between",
  },
  ".cm-sim-eval-row span": {
    color: "#94a3b8",
  },
  ".cm-sim-eval-row code": {
    color: "#f8fafc",
    fontFamily: "inherit",
  },
  ".cm-sim-eval-values": {
    display: "inline-flex",
    alignItems: "baseline",
    gap: "4px",
  },
  ".cm-sim-eval-prev": {
    color: "#94a3b8",
    textDecoration: "line-through",
    textDecorationColor: "rgba(148, 163, 184, 0.5)",
  },
  ".cm-sim-eval-arrow": {
    color: "#facc15",
    fontSize: "10px",
  },
  ".cm-sim-eval-footer": {
    marginTop: "4px",
    display: "flex",
    justifyContent: "space-between",
    alignItems: "center",
    gap: "8px",
  },
  ".cm-sim-eval-time": {
    fontSize: "10px",
    color: "#94a3b8",
    fontVariantNumeric: "tabular-nums",
  },
  ".cm-sim-eval-badge": {
    fontSize: "10px",
    padding: "1px 6px",
    borderRadius: "999px",
    textTransform: "uppercase",
    letterSpacing: "0.05em",
    fontWeight: "600",
  },
  ".cm-sim-eval-badge-live": {
    background: "rgba(250, 204, 21, 0.9)",
    color: "#0b1220",
  },
  ".cm-sim-eval-badge-stable": {
    background: "rgba(148, 163, 184, 0.25)",
    color: "#cbd5e1",
  },
  ".cm-sim-eval-name": {
    fontWeight: "600",
  },
});

export function debuggerExtension(config: DebuggerExtensionConfig): Extension {
  const field = activeSpanField(config);
  const driverField = driverHighlightsField(config);
  // BREAKPOINTS DISABLED FOR 0.3.0
  // Restore by un-commenting the bpField + breakpointsGutter lines below.
  // The state machinery (breakpointsField, BreakpointMarker, gutter,
  // CSS for .cm-breakpoint-*) is kept intact further down in this file.
  // const bpField = breakpointsField(config);
  return [
    field,
    driverField,
    // bpField,
    // breakpointsGutter(bpField, config),
    activeLinesGutter(field),
    storeBridge(config),
    buildHoverTooltip(config),
    debuggerBaseTheme,
  ];
}
