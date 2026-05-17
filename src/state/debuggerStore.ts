// TEMPORARY (v0.3.0): Breakpoint backend plumbing is disabled — the
// Tauri `sim_step` command no longer accepts a `breakpoints` argument.
// The local UI state (gutter markers, conditions in zustand) is retained
// so the in-progress feature work isn't lost; nothing reaches the
// simulator until breakpoint support is restored in verilog-core. Search
// for "BREAKPOINTS DISABLED FOR 0.3.0" in this file for the lines to
// uncomment.
//
import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/**
 * Time-synced source debugger client state.
 *
 * Backend-owned state (the live `SimSession`) is addressed by `sessionId`;
 * everything else in this store is local derived state the UI needs to
 * render without round-tripping.
 *
 * Time values are in femtoseconds (matching `Simulator::t_sim` in
 * `src-tauri/verilog-core/src/codegen.rs`), *not* the VCD tick units. The
 * trace already stores the femtosecond clock, but the VCD emits its own
 * tick units via `SimConfig::prec_fs` — so waveform cursor time (tick
 * units) and `activeTimeFs` (femtoseconds) don't match. We keep both so
 * the store can drive either lens without conversion until the backend
 * tells us the tick factor. See TODO in `seek` / `setPinnedTime`.
 */

/** Mirrors `ActiveSpan` in `src-tauri/src/sim_commands.rs`. */
export type ActiveSpan = {
  fileId: number;
  path: string;
  start: number;
  end: number;
  lineStart: number;
  colStart: number;
  lineEnd: number;
  colEnd: number;
};

export type SourceFileEntry = {
  id: number;
  path: string;
};

/** Matches the backend `StepMode` enum serialised as lowercase strings. */
export type StepMode = "statement" | "tick" | "cycle" | "run";

export type DebuggerMode = "stopped" | "paused" | "running";

type SimStartResult = {
  sessionId: number;
  vcdPath: string;
  sourceFiles: SourceFileEntry[];
  timeFs: number;
};

type StepResult = {
  timeFs: number;
  activeSpans: ActiveSpan[];
  ranStatements: number;
  done: boolean;
  // BREAKPOINTS DISABLED FOR 0.3.0 — restore when re-enabled.
  // breakpointHit: BreakpointHitDto | null;
  // breakpointWarnings: string[];
};

/**
 * Source-line breakpoint shown as a red gutter dot in the editor and
 * forwarded to the backend on every `sim_step`. `condition`, when set,
 * is a short expression parsed by the backend (`signal`, `signal == N`,
 * or `signal != N`); conditional breakpoints only halt when the
 * expression is true at the moment the statement fires.
 */
export type Breakpoint = {
  /** Client-side id — stable for the breakpoint's lifetime so the backend can reference the same row on hit. */
  id: number;
  path: string;
  /** 1-indexed line number. */
  line: number;
  /** Optional condition string, raw as-typed by the user. Empty string is treated as "no condition". */
  condition: string | null;
};

/** Matches `BreakpointHitDto` in `src-tauri/src/sim_commands.rs`. */
export type BreakpointHitDto = {
  breakpointId: number;
  timeFs: number;
  fileId: number;
  path: string;
  line: number;
};

export function breakpointKey(path: string, line: number): string {
  return `${path}:${line}`;
}

export type EvalResult = {
  decimal: string;
  hex: string;
  binary: string;
  width: number;
  resolvedName: string;
  /** True when a driver event fires for this signal at the pinned time. */
  transitioning: boolean;
  /** Previous value rendered in each base, only set when `transitioning`. */
  prevDecimal: string | null;
  prevHex: string | null;
  prevBinary: string | null;
};

/** Shape returned by the `sim_driver_at` Tauri command. */
export type DriverQueryResult = {
  resolvedSignal: string;
  fileId: number;
  path: string;
  stmtStart: number;
  stmtEnd: number;
  branchStart: number | null;
  branchEnd: number | null;
  condStart: number | null;
  condEnd: number | null;
  /** True when the backend only had statement-level info to return. */
  fallback: boolean;
};

/**
 * Precise source marks to overlay in the editor after the user explicitly
 * pressed "Jump to Driver" in the waveform. Only one driver can be
 * highlighted at a time; selecting a new one replaces the previous.
 */
export type DriverHighlights = {
  path: string;
  stmtStart: number;
  stmtEnd: number;
  branchStart: number | null;
  branchEnd: number | null;
  condStart: number | null;
  condEnd: number | null;
  signal: string;
  timeFs: number;
};

type StartArgs = {
  projectRoot: string;
  topModule?: string;
  numCycles?: number;
  vcdFilename?: string;
};

interface DebuggerState {
  sessionId: number | null;
  vcdPath: string | null;
  sourceFiles: SourceFileEntry[];
  /**
   * "Pinned" time in femtoseconds shared across editor and waveform. `null`
   * when no session is live.
   */
  pinnedTimeFs: number | null;
  /** Spans that fired at `pinnedTimeFs` (kept in sync with `pinnedTimeFs`). */
  activeSpans: ActiveSpan[];
  mode: DebuggerMode;
  /** Non-null while an async step/seek is in flight — used by the toolbar to disable buttons. */
  pending: boolean;
  /** Last error surfaced by the backend — consumed by a toast in `App.tsx`. */
  error: string | null;
  /** Bumped every time the backend flushes a fresh VCD so `WaveformPanel` can remount. */
  vcdTick: number;
  /** Current "Jump to Driver" highlight overlay, or null when not set. */
  driverHighlights: DriverHighlights | null;
  /** User breakpoints, keyed by `${path}:${line}`. Persisted to localStorage. */
  breakpoints: Record<string, Breakpoint>;
  /** Last-hit breakpoint from the most recent step — null between hits. */
  lastHit: BreakpointHitDto | null;

  start: (args: StartArgs) => Promise<SimStartResult | null>;
  step: (mode: StepMode, clockSignal?: string | null) => Promise<void>;
  seek: (timeFs: number) => Promise<void>;
  setPinnedTimeFs: (timeFs: number) => void;
  evalIdentifier: (
    identifier: string,
    filePath?: string | null,
    bytePos?: number | null,
  ) => Promise<EvalResult | null>;
  queryDriverAt: (
    signal: string,
    timeFs: number,
  ) => Promise<DriverQueryResult | null>;
  setDriverHighlights: (marks: DriverHighlights | null) => void;
  toggleBreakpoint: (path: string, line: number) => void;
  setBreakpointCondition: (path: string, line: number, condition: string | null) => void;
  removeBreakpoint: (path: string, line: number) => void;
  clearBreakpointsForFile: (path: string) => void;
  clearLastHit: () => void;
  stop: () => Promise<void>;
  clearError: () => void;
}

const BREAKPOINTS_STORAGE_KEY = "circuit-scope.breakpoints.v1";

function loadPersistedBreakpoints(): Record<string, Breakpoint> {
  if (typeof window === "undefined") return {};
  try {
    const raw = window.localStorage.getItem(BREAKPOINTS_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, Breakpoint>;
    // Shape-check so a malformed legacy value doesn't crash the store.
    const out: Record<string, Breakpoint> = {};
    for (const [k, v] of Object.entries(parsed ?? {})) {
      if (
        v &&
        typeof v === "object" &&
        typeof v.id === "number" &&
        typeof v.path === "string" &&
        typeof v.line === "number"
      ) {
        out[k] = {
          id: v.id,
          path: v.path,
          line: v.line,
          condition: typeof v.condition === "string" ? v.condition : null,
        };
      }
    }
    return out;
  } catch {
    return {};
  }
}

function persistBreakpoints(map: Record<string, Breakpoint>): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(BREAKPOINTS_STORAGE_KEY, JSON.stringify(map));
  } catch {
    /* quota/denied — non-fatal */
  }
}

let nextBreakpointId = 1;

export const useDebuggerStore = create<DebuggerState>((set, get) => {
  const initialBreakpoints = loadPersistedBreakpoints();
  // Seed the id counter past any persisted id so newly-created breakpoints
  // never collide with restored ones.
  for (const bp of Object.values(initialBreakpoints)) {
    if (bp.id >= nextBreakpointId) nextBreakpointId = bp.id + 1;
  }
  return ({
  sessionId: null,
  vcdPath: null,
  sourceFiles: [],
  pinnedTimeFs: null,
  activeSpans: [],
  mode: "stopped",
  pending: false,
  error: null,
  vcdTick: 0,
  driverHighlights: null,
  breakpoints: initialBreakpoints,
  lastHit: null,

  async start({ projectRoot, topModule, numCycles, vcdFilename }) {
    const prev = get().sessionId;
    if (prev != null) {
      try {
        await invoke("sim_end", { sessionId: prev });
      } catch {
        /* best-effort cleanup */
      }
    }
    set({ pending: true, error: null });
    try {
      const res = await invoke<SimStartResult>("sim_start", {
        args: {
          projectRoot,
          topModule: topModule ?? null,
          numCycles: numCycles ?? null,
          vcdFilename: vcdFilename ?? null,
        },
      });
      set({
        sessionId: res.sessionId,
        vcdPath: res.vcdPath,
        sourceFiles: res.sourceFiles,
        pinnedTimeFs: res.timeFs,
        activeSpans: [],
        mode: "paused",
        pending: false,
        vcdTick: get().vcdTick + 1,
      });
      return res;
    } catch (e) {
      set({ pending: false, error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  async step(mode, clockSignal) {
    const sessionId = get().sessionId;
    if (sessionId == null) return;
    set({ pending: true, error: null, mode: mode === "run" ? "running" : "paused" });
    try {
      // BREAKPOINTS DISABLED FOR 0.3.0
      // -----------------------------------------------------------------
      // The Tauri `sim_step` command no longer accepts a `breakpoints`
      // argument while the breakpoint feature is being reworked. When
      // re-enabling, restore the block below and uncomment the
      // `breakpoints` field on the invoke call + the warning/lastHit
      // handling. See verilog-core/src/sim_session.rs for the underlying
      // backend pieces that also need to come back.
      //
      // const bpList = Object.values(get().breakpoints).map((bp) => ({
      //   id: bp.id,
      //   path: bp.path,
      //   line: bp.line,
      //   condition: bp.condition,
      // }));
      const res = await invoke<StepResult>("sim_step", {
        sessionId,
        mode,
        clockSignal: clockSignal ?? null,
        // breakpoints: bpList,
      });
      set({
        pinnedTimeFs: res.timeFs,
        activeSpans: res.activeSpans,
        // Breakpoint hits previously paused regardless of mode; restore
        // that behaviour when breakpoints come back.
        mode: res.done ? "stopped" : "paused",
        pending: false,
        vcdTick: get().vcdTick + 1,
        // lastHit: res.breakpointHit ?? null,
      });
      // if (res.breakpointWarnings && res.breakpointWarnings.length > 0) {
      //   set({ error: res.breakpointWarnings.join("\n") });
      // }
    } catch (e) {
      set({ pending: false, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async seek(timeFs) {
    const sessionId = get().sessionId;
    if (sessionId == null) {
      set({ pinnedTimeFs: timeFs });
      return;
    }
    set({ pinnedTimeFs: timeFs, pending: true });
    try {
      const spans = await invoke<ActiveSpan[]>("sim_seek", { sessionId, timeFs });
      set({ activeSpans: spans, pending: false });
    } catch (e) {
      set({ pending: false, error: e instanceof Error ? e.message : String(e) });
    }
  },

  setPinnedTimeFs(timeFs) {
    void get().seek(timeFs);
  },

  async evalIdentifier(identifier, filePath, bytePos) {
    const { sessionId, pinnedTimeFs } = get();
    if (sessionId == null) return null;
    try {
      const res = await invoke<EvalResult>("sim_eval", {
        args: {
          sessionId,
          identifier,
          filePath: filePath ?? null,
          timeFs: pinnedTimeFs ?? null,
          bytePos: bytePos ?? null,
        },
      });
      return res;
    } catch {
      // Hover failures are expected for non-signal identifiers; swallow.
      return null;
    }
  },

  async queryDriverAt(signal, timeFs) {
    const sessionId = get().sessionId;
    if (sessionId == null) return null;
    try {
      const res = await invoke<DriverQueryResult>("sim_driver_at", {
        sessionId,
        signal,
        timeFs,
      });
      return res;
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  setDriverHighlights(marks) {
    set({ driverHighlights: marks });
  },

  toggleBreakpoint(path, line) {
    const key = breakpointKey(path, line);
    const current = get().breakpoints;
    const next = { ...current };
    const existed = key in next;
    if (key in next) {
      delete next[key];
    } else {
      next[key] = { id: nextBreakpointId++, path, line, condition: null };
    }
    persistBreakpoints(next);
    set({ breakpoints: next });
  },

  setBreakpointCondition(path, line, condition) {
    const key = breakpointKey(path, line);
    const current = get().breakpoints;
    const existing = current[key];
    if (!existing) {
      // Setting a condition implicitly creates the breakpoint.
      const next = {
        ...current,
        [key]: {
          id: nextBreakpointId++,
          path,
          line,
          condition: condition && condition.trim().length > 0 ? condition : null,
        },
      };
      persistBreakpoints(next);
      set({ breakpoints: next });
      return;
    }
    const next = {
      ...current,
      [key]: {
        ...existing,
        condition: condition && condition.trim().length > 0 ? condition : null,
      },
    };
    persistBreakpoints(next);
    set({ breakpoints: next });
  },

  removeBreakpoint(path, line) {
    const key = breakpointKey(path, line);
    const current = get().breakpoints;
    if (!(key in current)) return;
    const next = { ...current };
    delete next[key];
    persistBreakpoints(next);
    set({ breakpoints: next });
  },

  clearBreakpointsForFile(path) {
    const current = get().breakpoints;
    const next: Record<string, Breakpoint> = {};
    let changed = false;
    for (const [k, v] of Object.entries(current)) {
      if (v.path === path) {
        changed = true;
        continue;
      }
      next[k] = v;
    }
    if (!changed) return;
    persistBreakpoints(next);
    set({ breakpoints: next });
  },

  clearLastHit() {
    set({ lastHit: null });
  },

  async stop() {
    const sessionId = get().sessionId;
    if (sessionId == null) return;
    set({ pending: true });
    // Close the waveform viewer first so the file handle is released on
    // platforms (Windows) that hold it exclusively — then `sim_end` can
    // delete the VCD cleanly.
    try {
      await invoke("vcd_close");
    } catch {
      /* best-effort */
    }
    try {
      await invoke("sim_end", { sessionId });
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
    } finally {
      set({
        sessionId: null,
        vcdPath: null,
        sourceFiles: [],
        pinnedTimeFs: null,
        activeSpans: [],
        mode: "stopped",
        pending: false,
        driverHighlights: null,
      });
    }
  },

  clearError() {
    set({ error: null });
  },
  });
});

/**
 * Subscribe-style helper for non-React callers (e.g. the CodeMirror
 * extension). Returns an unsubscribe function.
 */
export function subscribeDebugger(
  listener: (state: DebuggerState) => void,
): () => void {
  return useDebuggerStore.subscribe(listener);
}
