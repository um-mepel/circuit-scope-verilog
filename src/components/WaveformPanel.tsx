import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowBigLeft,
  ArrowBigRight,
  ArrowLeft,
  ArrowRight,
  Binary,
  Calculator,
  FastForward,
  Hash,
  Maximize2,
  Rewind,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import { theme } from "../ui/theme";
import { IconButton } from "./IconButton";
import type { ToastKind } from "./ToastStack";
import { useDebuggerStore } from "../state/debuggerStore";

/**
 * Strictly increasing id for each `vcd_open`, safe for Rust `u64` over Tauri IPC.
 * Combines wall time with a local increment so that after **hot reload** (module state resets
 * to 0) we still exceed the backend's persisted `max_open_seq`; otherwise every open is
 * superseded and the waveform never loads.
 */
let lastWaveformVcdOpenSeq = 0;

function allocWaveformVcdOpenSeq(): number {
  const t = Date.now();
  lastWaveformVcdOpenSeq = Math.max(lastWaveformVcdOpenSeq + 1, t);
  return lastWaveformVcdOpenSeq;
}

type VcdScopeNode = {
  name: string;
  scopes: VcdScopeNode[];
  vars: { signalId: number; name: string; bits: number }[];
};

type VcdOpenResponse = {
  /** Backend ignored this open (newer open or close superseded it). */
  superseded?: boolean;
  path: string;
  timeStart: number;
  timeEnd: number;
  timescaleFactor: number | null;
  timescaleUnit: string | null;
  hierarchy: VcdScopeNode[];
};

type VcdTransition = {
  signalId: number;
  time: number;
  value: string;
};

type Props = {
  projectRoot: string;
  vcdPath: string;
  onClose: () => void;
  onToast?: (kind: ToastKind, message: string) => void;
};

const ZOOM_STEP = 1.15;
const SNAP_PX = 8;
/** Row height in CSS px (must match hit-testing). */
const ROW_H = 42;
/** Time ruler height at top of plot (matches values-column spacer). */
const RULER_H = 52;
/** Y (top baseline) for the orange sample-time readout row. */
const RULER_SAMPLE_Y = 4;
/** Width of trace names + value column (between scope tree and canvas). */
const VALUES_COL_W = 176;
/** Target major tick count; actual count follows “nice” step sizing. */
const RULER_TICKS = 9;
const PAN_THRESHOLD_PX = 5;
/** Trackpad horizontal swipe pan sensitivity (1 ≈ same scale as drag). */
const SWIPE_PAN_FACTOR = 1;

type BusRadix = "hex" | "bin" | "dec";

const wf = theme.waveform;
const COL_BG = wf.bg;
const COL_ROW_BG = wf.rowBg;
const COL_ROW_ALT = wf.rowAlt;
const COL_GRID = wf.grid;
const COL_BUS_FILL = wf.busFill;
const COL_BUS_STROKE = wf.busStroke;
const COL_WIRE = wf.wire;
const COL_WIRE_HILO = wf.wireHiLo;
const COL_FOCUS_BAR = wf.focusBar;
/** Values column: focus band left of the waveform divider. */
const COL_FOCUS_VALUES_BG = wf.focusValuesBg;
const COL_LABEL = wf.label;
const COL_RULER_BG = wf.rulerBg;
const COL_RULER_TICK = wf.rulerTick;
const COL_RULER_TEXT = wf.rulerText;
const COL_ROW_STROKE = wf.rowStroke;
const COL_SAMPLE_LINE = wf.sampleLine;
const COL_SAMPLE_TAG = wf.sampleTag;
/** Dashed hover cursor drawn on top of the pinned cursor. */
const COL_HOVER_LINE = "rgba(148, 163, 184, 0.75)";

/** Rounds span/target into 1–2–5×10ⁿ tick spacing (scales with zoom). */
function niceStepForSpan(span: number, targetDivisions: number): number {
  if (!Number.isFinite(span) || span <= 0) return 1;
  const rough = span / Math.max(2, targetDivisions);
  if (!(rough > 0)) return 1;
  const exp = Math.floor(Math.log10(rough));
  const f = rough / 10 ** exp;
  const nf = f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10;
  return nf * 10 ** exp;
}

function buildRulerTicks(
  t0: number,
  t1: number,
  maxTicks: number,
): { times: number[]; step: number } {
  const lo = Math.min(t0, t1);
  const hi = Math.max(t0, t1);
  const span = Math.max(1e-30, hi - lo);
  const step = niceStepForSpan(span, maxTicks);
  const times: number[] = [];
  const k0 = Math.ceil((lo - step * 1e-12) / step);
  const tStart = k0 * step;
  const nMax = Math.min(maxTicks + 15, Math.ceil((hi - tStart) / step) + 2);
  for (let k = 0; k <= nMax; k++) {
    const t = tStart + k * step;
    if (t > hi + step * 1e-9) break;
    if (t + 1e-9 >= lo && t - 1e-9 <= hi) times.push(t);
  }
  if (times.length === 0) times.push(lo);
  return { times, step };
}

function formatNiceTick(t: number, step: number): string {
  if (!Number.isFinite(t) || !Number.isFinite(step)) return "";
  const s = Math.abs(step);
  if (s <= 0) return String(t);
  const use = Number((Math.round(t / s) * s).toFixed(12));
  const mag = Math.max(Math.abs(use), s);
  if (mag >= 1e15) return use.toExponential(2);
  const ord = Math.floor(Math.log10(s));
  if (ord >= 3) return String(Math.round(use));
  if (ord >= 0) {
    if (s >= 10) return String(Math.round(use));
    return Number.isInteger(use) ? String(Math.round(use)) : use.toFixed(1).replace(/\.0$/, "");
  }
  const decimals = Math.min(6, Math.max(0, -ord));
  return use.toFixed(decimals).replace(/\.?0+$/, "").replace(/\.$/, "") || "0";
}

/** Sample readout: same “grain” as ruler, with one extra digit when zoomed in. */
function formatSampleTimeLabel(t: number, rulerStep: number): string {
  if (!Number.isFinite(t)) return "";
  const s = Math.abs(rulerStep);
  if (!(s > 0)) return String(t);
  let d = 0;
  if (s < 1) d = Math.min(8, Math.ceil(-Math.log10(s)) + 1);
  else if (s < 10) d = 1;
  const r = Number(t.toFixed(Math.min(8, d)));
  if (Number.isInteger(r)) return String(r);
  return String(r).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

/** One VCD `#` step length in seconds: `factor` from `$timescale` × SI base of the unit. */
function vcdSecondsPerTick(meta: VcdOpenResponse | null): number | null {
  const f = meta?.timescaleFactor;
  const unitRaw = meta?.timescaleUnit?.trim().toLowerCase() ?? "";
  if (f == null || !Number.isFinite(f) || f <= 0 || !unitRaw) return null;
  const unitToSec: Record<string, number> = {
    zs: 1e-21,
    as: 1e-18,
    fs: 1e-15,
    ps: 1e-12,
    ns: 1e-9,
    us: 1e-6,
    μs: 1e-6,
    "\u00b5s": 1e-6,
    ms: 1e-3,
    s: 1,
  };
  const base = unitToSec[unitRaw];
  if (base == null || unitRaw === "unknown") return null;
  return f * base;
}

/** Pick a single SI suffix for the visible window (same idea as VaporView / Surfer). */
function timeDisplayScaleForSpan(spanSeconds: number): { divisor: number; suffix: string } {
  if (!Number.isFinite(spanSeconds) || spanSeconds <= 0) {
    return { divisor: 1e-9, suffix: "ns" };
  }
  let expTier = Math.floor(Math.log10(spanSeconds) / 3) * 3;
  expTier = Math.max(-21, Math.min(3, expTier));
  const divisor = 10 ** expTier;
  const suffix =
    expTier === 3
      ? "ks"
      : expTier === 0
        ? "s"
        : expTier === -3
          ? "ms"
          : expTier === -6
            ? "\u00b5s"
            : expTier === -9
              ? "ns"
              : expTier === -12
                ? "ps"
                : expTier === -15
                  ? "fs"
                  : expTier === -18
                    ? "as"
                    : "zs";
  return { divisor, suffix };
}

function formatRulerTickLabel(
  tick: number,
  tickStep: number,
  meta: VcdOpenResponse | null,
  viewT0: number,
  viewT1: number,
): string {
  const secPerTick = vcdSecondsPerTick(meta);
  if (secPerTick == null) return formatNiceTick(tick, tickStep);
  const lo = Math.min(viewT0, viewT1);
  const hi = Math.max(viewT0, viewT1);
  const spanTicks = Math.max(hi - lo, 1);
  const spanSec = spanTicks * secPerTick;
  const { divisor, suffix } = timeDisplayScaleForSpan(spanSec);
  const tSec = tick * secPerTick;
  const stepSec = tickStep * secPerTick;
  return `${formatNiceTick(tSec / divisor, stepSec / divisor)} ${suffix}`;
}

function formatSampleTimeLabelWithMeta(
  tick: number,
  tickStep: number,
  meta: VcdOpenResponse | null,
  viewT0: number,
  viewT1: number,
): string {
  const secPerTick = vcdSecondsPerTick(meta);
  if (secPerTick == null) return formatSampleTimeLabel(tick, tickStep);
  const lo = Math.min(viewT0, viewT1);
  const hi = Math.max(viewT0, viewT1);
  const spanTicks = Math.max(hi - lo, 1);
  const spanSec = spanTicks * secPerTick;
  const { divisor, suffix } = timeDisplayScaleForSpan(spanSec);
  const tSec = tick * secPerTick;
  const stepSec = tickStep * secPerTick;
  return `${formatSampleTimeLabel(tSec / divisor, stepSec / divisor)} ${suffix}`;
}

function formatVcdTickForTooltip(
  tick: number | null,
  meta: VcdOpenResponse | null,
  viewT0: number,
  viewT1: number,
): string {
  if (tick == null || !Number.isFinite(tick)) return "—";
  const secPerTick = vcdSecondsPerTick(meta);
  if (secPerTick == null) return String(tick);
  const lo = Math.min(viewT0, viewT1);
  const hi = Math.max(viewT0, viewT1);
  const spanTicks = Math.max(hi - lo, 1);
  const spanSec = spanTicks * secPerTick;
  const { divisor, suffix } = timeDisplayScaleForSpan(spanSec);
  const tSec = tick * secPerTick;
  const v = tSec / divisor;
  const label = Number.isInteger(v)
    ? String(v)
    : v.toFixed(9).replace(/\.?0+$/, "").replace(/\.$/, "");
  return `${label} ${suffix}`;
}

function timeBounds(meta: VcdOpenResponse): { tMin: number; tMax: number } {
  const tMin = Math.min(meta.timeStart, meta.timeEnd);
  const tMaxRaw = Math.max(meta.timeStart, meta.timeEnd);
  const tMax = tMaxRaw > tMin ? tMaxRaw : tMin + 1;
  return { tMin, tMax };
}

function clampViewToMeta(v0: number, v1: number, meta: VcdOpenResponse): [number, number] {
  const { tMin, tMax } = timeBounds(meta);
  let a = Math.min(v0, v1);
  let b = Math.max(v0, v1);
  let span = Math.max(1, b - a);
  const maxSpan = Math.max(1, tMax - tMin);
  if (span > maxSpan) span = maxSpan;
  if (a < tMin) {
    a = tMin;
    b = a + span;
  }
  if (b > tMax) {
    b = tMax;
    a = b - span;
    if (a < tMin) a = tMin;
  }
  return [a, b];
}

function queryTimeWindow(
  tView0: number,
  tView1: number,
  meta: VcdOpenResponse,
): { tStart: number; tEnd: number } {
  const [a, b] = clampViewToMeta(tView0, tView1, meta);
  const tStart = Math.floor(Math.min(a, b));
  let tEnd = Math.ceil(Math.max(a, b));
  if (tEnd <= tStart) tEnd = tStart + 1;
  return { tStart, tEnd };
}

function walkVars(
  meta: VcdOpenResponse,
): Map<number, { name: string; bits: number; path: string }> {
  const m = new Map<number, { name: string; bits: number; path: string }>();
  const walk = (n: VcdScopeNode, prefix: string) => {
    const here = prefix.length > 0 ? `${prefix}.${n.name}` : n.name;
    for (const v of n.vars) {
      m.set(v.signalId, {
        name: v.name,
        bits: v.bits,
        path: here.length > 0 ? `${here}.${v.name}` : v.name,
      });
    }
    for (const s of n.scopes) walk(s, here);
  };
  for (const root of meta.hierarchy) walk(root, "");
  return m;
}

/** `signalId` = whole bus; `signalId:bN` = bit N (LSB = 0). */
type TraceKey = string;

type PersistedWaveformUiState = {
  selected: TraceKey[];
  focusTraceKey: TraceKey | null;
  expandedBusKeys: string[];
  collapsedScopePaths: string[];
};

const waveformUiStateByPath = new Map<string, PersistedWaveformUiState>();

function uiStateStorageKey(vcdPath: string): string {
  return `circuitscope_waveform_ui_${vcdPath}`;
}

function loadPersistedUiState(vcdPath: string): PersistedWaveformUiState | null {
  const mem = waveformUiStateByPath.get(vcdPath);
  if (mem) return mem;
  try {
    const raw = window.localStorage.getItem(uiStateStorageKey(vcdPath));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as PersistedWaveformUiState;
    if (!parsed || !Array.isArray(parsed.selected)) return null;
    return parsed;
  } catch {
    return null;
  }
}

function savePersistedUiState(vcdPath: string, state: PersistedWaveformUiState): void {
  waveformUiStateByPath.set(vcdPath, state);
  try {
    window.localStorage.setItem(uiStateStorageKey(vcdPath), JSON.stringify(state));
  } catch {
    // ignore quota/storage issues
  }
}

function traceKeyWhole(signalId: number): TraceKey {
  return String(signalId);
}

function traceKeyBit(signalId: number, lsbIndex: number): TraceKey {
  return `${signalId}:b${lsbIndex}`;
}

function parseTraceKey(key: TraceKey): { signalId: number; bit: number | null } {
  const m = /^(\d+)(:b(\d+))?$/.exec(key);
  if (!m) return { signalId: 0, bit: null };
  return { signalId: Number(m[1]), bit: m[3] != null ? Number(m[3]) : null };
}

function compareTraceKey(a: TraceKey, b: TraceKey): number {
  const pa = parseTraceKey(a);
  const pb = parseTraceKey(b);
  if (pa.signalId !== pb.signalId) return pa.signalId - pb.signalId;
  if (pa.bit === null && pb.bit === null) return 0;
  if (pa.bit === null) return -1;
  if (pb.bit === null) return 1;
  return pa.bit - pb.bit;
}

/** MSB-first binary string; `lsbIndex` 0 = rightmost bit. */
function extractLsbIndexBit(raw: string, lsbIndex: number, width: number): string {
  const t = raw.trim();
  if (/^[01xzXZ?]+$/.test(t) && t.length > 0) {
    const bin = t.length >= width ? t.slice(-width) : t.padStart(width, "0");
    const ci = width - 1 - lsbIndex;
    if (ci < 0 || ci >= bin.length) return "0";
    return bin[ci]!;
  }
  const hexFlat = t.replace(/^0x/i, "").replace(/_/g, "");
  if (/^[0-9a-fA-F]+$/.test(hexFlat) && hexFlat.length <= 32) {
    try {
      const n = BigInt("0x" + hexFlat);
      const bit = (n >> BigInt(lsbIndex)) & 1n;
      return bit === 1n ? "1" : "0";
    } catch {
      /* ignore */
    }
  }
  if (t === "0" || t === "1") return lsbIndex === 0 ? t : "0";
  return t.length > 0 ? t[0]! : "0";
}

function projectTransitionsToBit(
  list: VcdTransition[],
  lsbIndex: number,
  width: number,
): VcdTransition[] {
  if (list.length === 0) return [];
  const sorted = [...list].sort((a, b) => a.time - b.time);
  const out: VcdTransition[] = [];
  let lastEmit: string | null = null;
  for (const tr of sorted) {
    const b = extractLsbIndexBit(tr.value, lsbIndex, width);
    if (lastEmit === null || b !== lastEmit) {
      out.push({ signalId: tr.signalId, time: tr.time, value: b });
      lastEmit = b;
    }
  }
  return out;
}

function transitionsForTraceKey(
  traceKey: TraceKey,
  bySig: Map<number, VcdTransition[]>,
  signalInfo: Map<number, { name: string; bits: number; path: string }>,
): VcdTransition[] {
  const { signalId, bit } = parseTraceKey(traceKey);
  const raw = bySig.get(signalId) ?? [];
  if (bit == null) return raw;
  const w = signalInfo.get(signalId)?.bits ?? 1;
  return projectTransitionsToBit(raw, bit, Math.max(1, w));
}

function traceDisplayName(
  traceKey: TraceKey,
  signalInfo: Map<number, { name: string; bits: number; path: string }>,
): string {
  const { signalId, bit } = parseTraceKey(traceKey);
  const info = signalInfo.get(signalId);
  const base = info?.name ?? `#${signalId}`;
  if (bit == null) return base;
  return `${base}[${bit}]`;
}

function availableTraceKeys(meta: VcdOpenResponse): Set<TraceKey> {
  const out = new Set<TraceKey>();
  const walk = (n: VcdScopeNode) => {
    for (const v of n.vars) {
      out.add(traceKeyWhole(v.signalId));
      if (v.bits > 1) {
        for (let bi = 0; bi < v.bits; bi++) out.add(traceKeyBit(v.signalId, bi));
      }
    }
    for (const s of n.scopes) walk(s);
  };
  for (const root of meta.hierarchy) walk(root);
  return out;
}

/** Sidebar: hierarchy + visibility checkboxes; nested scopes can collapse; buses decompose per-bit. */
function ScopeTree({
  node,
  depth,
  scopePath,
  collapsedScopePaths,
  onToggleScopeCollapse,
  expandedBusKeys,
  onToggleBusExpand,
  selected,
  focusTraceKey,
  toggle,
  signalInfo,
}: {
  node: VcdScopeNode;
  depth: number;
  scopePath: string;
  collapsedScopePaths: Set<string>;
  onToggleScopeCollapse: (path: string) => void;
  expandedBusKeys: Set<string>;
  onToggleBusExpand: (busKey: string) => void;
  selected: Set<TraceKey>;
  focusTraceKey: TraceKey | null;
  toggle: (key: TraceKey) => void;
  signalInfo: Map<number, { name: string; bits: number; path: string }>;
}) {
  const nested = depth > 0;
  const hasBody = node.vars.length > 0 || node.scopes.length > 0;
  const showChevron = nested && hasBody;
  const collapsed = collapsedScopePaths.has(scopePath);

  return (
    <div style={{ marginLeft: depth * 10 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 2, minHeight: 20 }}>
        {showChevron ? (
          <button
            type="button"
            aria-expanded={!collapsed}
            aria-label={collapsed ? "Expand scope" : "Collapse scope"}
            title={collapsed ? "Expand" : "Collapse"}
            onClick={(e) => {
              e.preventDefault();
              onToggleScopeCollapse(scopePath);
            }}
            style={{
              flexShrink: 0,
              width: 20,
              height: 20,
              padding: 0,
              border: `1px solid ${theme.waveform.chevronBtnBorder}`,
              borderRadius: theme.radius.sm - 1,
              background: theme.waveform.chevronBtnBg,
              color: theme.text.secondary,
              cursor: "pointer",
              fontSize: 9,
              lineHeight: 1,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
            }}
          >
            {collapsed ? "▶" : "▼"}
          </button>
        ) : nested ? (
          <span style={{ width: 20, flexShrink: 0 }} aria-hidden />
        ) : null}
        <div
          style={{
            fontSize: depth === 0 ? 13 : 12,
            color:
              depth === 0
                ? theme.waveform.scopeHeaderTop
                : theme.waveform.scopeHeaderNested,
            padding: "3px 0",
            fontWeight: depth === 0 ? 700 : 600,
            letterSpacing: "0.04em",
            flex: 1,
            minWidth: 0,
          }}
        >
          {node.name || "(root)"}
        </div>
      </div>
      {!collapsed && (
        <>
          {node.vars.map((v) => {
            const wholeKey = traceKeyWhole(v.signalId);
            const busKey = `${scopePath}-bus${v.signalId}`;
            const busExpanded = expandedBusKeys.has(busKey);
            const w = signalInfo.get(v.signalId)?.bits ?? v.bits;
            if (v.bits <= 1) {
              const focused = focusTraceKey === wholeKey;
              return (
                <label
                  key={v.signalId}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 6,
                    fontSize: 11,
                    fontWeight: 400,
                    color: focused ? theme.waveform.scopeVarFocus : theme.waveform.scopeVar,
                    cursor: "pointer",
                    padding: "2px 6px",
                    margin: "1px 0",
                    borderRadius: theme.radius.sm,
                    background: focused ? theme.waveform.focusRowTint : "transparent",
                  }}
                >
                  <input
                    type="checkbox"
                    className="cs-waveform-checkbox"
                    checked={selected.has(wholeKey)}
                    onChange={() => toggle(wholeKey)}
                  />
                  <span style={{ userSelect: "none" }}>{v.name}</span>
                </label>
              );
            }
            const busFocus =
              focusTraceKey === wholeKey ||
              (focusTraceKey != null &&
                parseTraceKey(focusTraceKey).signalId === v.signalId &&
                parseTraceKey(focusTraceKey).bit != null);
            return (
              <div key={v.signalId} style={{ margin: "3px 0 4px" }}>
                <div style={{ display: "flex", alignItems: "center", gap: 2 }}>
                  <label
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 6,
                      fontSize: 11,
                      fontWeight: 400,
                      color: busFocus ? theme.waveform.scopeVarFocus : theme.waveform.scopeVar,
                      cursor: "pointer",
                      padding: "2px 4px",
                      margin: 0,
                      borderRadius: theme.radius.sm,
                      background:
                        focusTraceKey === wholeKey ? theme.waveform.focusRowTint : "transparent",
                      flex: 1,
                      minWidth: 0,
                    }}
                  >
                    <input
                      type="checkbox"
                      className="cs-waveform-checkbox"
                      checked={selected.has(wholeKey)}
                      onChange={() => toggle(wholeKey)}
                    />
                    <span style={{ userSelect: "none" }}>
                      {v.name}
                      <span style={{ opacity: 0.45, marginLeft: 4 }}>
                        [{v.bits - 1}:0]
                      </span>
                    </span>
                  </label>
                  <button
                    type="button"
                    aria-expanded={busExpanded}
                    aria-label={busExpanded ? "Hide bits" : "Show bits"}
                    title={busExpanded ? "Hide single-bit traces" : "Decompose into bits"}
                    onClick={(e) => {
                      e.preventDefault();
                      onToggleBusExpand(busKey);
                    }}
                    style={{
                      flexShrink: 0,
                      padding: "0 2px",
                      margin: 0,
                      border: "none",
                      background: "transparent",
                      color: theme.text.secondary,
                      cursor: "pointer",
                      fontSize: 10,
                      lineHeight: 1,
                    }}
                  >
                    {busExpanded ? "▼" : "▶"}
                  </button>
                </div>
                {busExpanded ? (
                  <div style={{ marginLeft: 22, marginTop: 4 }}>
                    {Array.from({ length: w }, (_, bi) => {
                      const bk = traceKeyBit(v.signalId, bi);
                      const bitFocused = focusTraceKey === bk;
                      return (
                        <label
                          key={bk}
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: 6,
                            fontSize: 11,
                            fontWeight: 400,
                            color: bitFocused ? theme.waveform.scopeVarFocus : theme.waveform.scopeVar,
                            cursor: "pointer",
                            padding: "2px 6px",
                            margin: "1px 0",
                            borderRadius: theme.radius.sm,
                            background: bitFocused ? theme.waveform.focusRowTintSoft : "transparent",
                          }}
                        >
                          <input
                            type="checkbox"
                            className="cs-waveform-checkbox"
                            checked={selected.has(bk)}
                            onChange={() => toggle(bk)}
                          />
                          <span
                            style={{
                              userSelect: "none",
                              fontFamily: theme.font.mono,
                            }}
                          >
                            {v.name}[{bi}]
                          </span>
                        </label>
                      );
                    })}
                  </div>
                ) : null}
              </div>
            );
          })}
          {node.scopes.map((s, i) => (
            <ScopeTree
              key={`${scopePath}/s${i}`}
              node={s}
              depth={depth + 1}
              scopePath={`${scopePath}/s${i}`}
              collapsedScopePaths={collapsedScopePaths}
              onToggleScopeCollapse={onToggleScopeCollapse}
              expandedBusKeys={expandedBusKeys}
              onToggleBusExpand={onToggleBusExpand}
              selected={selected}
              focusTraceKey={focusTraceKey}
              toggle={toggle}
              signalInfo={signalInfo}
            />
          ))}
        </>
      )}
    </div>
  );
}

/** Format VCD value strings for multi-bit (and 1-bit) display. */
function formatValueDisplay(s: string, radix: BusRadix): string {
  const t = s.trim();

  if (radix === "bin") {
    if (/^[01]+$/.test(t)) return t;
    if (/^[01xzXZ?]+$/.test(t)) return t.length > 28 ? `${t.slice(0, 25)}…` : t;
    return t.length > 20 ? `${t.slice(0, 17)}…` : t;
  }

  if (radix === "dec") {
    if (/^[01]+$/.test(t) && t.length <= 128) {
      try {
        let n = 0n;
        for (let i = 0; i < t.length; i++) {
          n = (n << 1n) | BigInt(t[i] === "1" ? 1 : 0);
        }
        return n.toString(10);
      } catch {
        return t;
      }
    }
    if (/^[01]+$/.test(t)) {
      const n = parseInt(t, 2);
      return Number.isFinite(n) ? String(n) : t;
    }
    if (/^[01xzXZ?]+$/.test(t)) return t.length > 16 ? `${t.slice(0, 13)}…` : t;
    return t.length > 18 ? `${t.slice(0, 15)}…` : t;
  }

  if (/^[01]+$/.test(t) && t.length >= 1 && t.length <= 64) {
    const n = parseInt(t, 2);
    const hex = n.toString(16).toUpperCase();
    const w = Math.ceil(t.length / 4);
    return `0x${hex.padStart(Math.max(1, w), "0")}`;
  }
  if (/^[01xzXZ?]+$/.test(t) && t.length >= 2) {
    return t.length > 14 ? `${t.slice(0, 11)}…` : t;
  }
  return t.length > 18 ? `${t.slice(0, 15)}…` : t;
}

function parseBinLevel(s: string): 0 | 1 | null {
  const t = s.trim();
  if (t === "0" || t === "1") return Number(t) as 0 | 1;
  if (t.length === 1 && (t[0] === "0" || t[0] === "1")) return Number(t) as 0 | 1;
  return null;
}

function isMultiBitStyle(bits: number, sampleVal: string): boolean {
  if (bits > 1) return true;
  const t = sampleVal.trim();
  return t.length > 1 && /^[01xzXZ?]+$/.test(t);
}

/** Latest value at or before `t` from sorted transitions for one signal (window-local data). */
function valueAtTime(sortedAsc: VcdTransition[], t: number | null): string | null {
  if (t == null) return null;
  if (sortedAsc.length === 0) return null;
  let last: string | null = null;
  for (const tr of sortedAsc) {
    if (tr.time > t) break;
    last = tr.value;
  }
  return last;
}

type BusSegment = { x0: number; x1: number; val: string };

/** Stable value intervals for VaporView-style bus strips (piecewise constant). */
function buildBusSegments(
  list: VcdTransition[],
  t0: number,
  t1: number,
  tToX: (t: number) => number,
  w: number,
): BusSegment[] {
  if (list.length === 0) return [];
  const sorted = [...list].sort((a, b) => a.time - b.time);
  let curVal = sorted[0]!.value;
  for (const tr of sorted) {
    if (tr.time <= t0) curVal = tr.value;
    else break;
  }
  const segs: BusSegment[] = [];
  let xLeft = 0;
  let val = curVal;
  for (const tr of sorted) {
    if (tr.time < t0) {
      val = tr.value;
      continue;
    }
    if (tr.time > t1) break;
    const x = tToX(tr.time);
    const xClamped = Math.max(0, Math.min(w, x));
    if (xClamped > xLeft + 0.25) {
      segs.push({ x0: xLeft, x1: xClamped, val });
    }
    xLeft = xClamped;
    val = tr.value;
  }
  if (w > xLeft + 0.25) {
    segs.push({ x0: xLeft, x1: w, val });
  }
  return segs;
}

export function WaveformPanel({ projectRoot, vcdPath, onClose: _onClose, onToast }: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const [meta, setMeta] = useState<VcdOpenResponse | null>(null);
  const [selected, setSelected] = useState<Set<TraceKey>>(new Set());
  /** Trace selected on canvas; drives edge-snap and row highlight. */
  const [focusTraceKey, setFocusTraceKey] = useState<TraceKey | null>(null);
  /** Multi-bit vars with ▶ expanded to show per-bit checkboxes (sidebar). */
  const [expandedBusKeys, setExpandedBusKeys] = useState<Set<string>>(() => new Set());
  const [transitions, setTransitions] = useState<VcdTransition[]>([]);
  const [busRadix, setBusRadix] = useState<BusRadix>("hex");
  /** Nested scope paths (not top-level roots) hidden when collapsed. */
  const [collapsedScopePaths, setCollapsedScopePaths] = useState<Set<string>>(() => new Set());

  const [tView0, setTView0] = useState(0);
  const [tView1, setTView1] = useState(1);
  /** Plot area width in CSS px; drives per-column VCD resampling so the full trace matches the canvas. */
  const [plotCssWidth, setPlotCssWidth] = useState(1024);

  const pointerRef = useRef({
    down: false,
    clientStartX: 0,
    clientStartY: 0,
    canvasMy: 0,
    panning: false,
    lastX: 0,
  });

  /**
   * Local hover-time in VCD tick units. Drawn as a dashed "ghost" cursor.
   * Cleared on mouse leave.
   */
  const [hoverTime, setHoverTime] = useState<number | null>(null);
  /**
   * Local fallback pin (tick units) used when no debugger session is active or
   * the VCD has no parseable timescale. When a debug session is live, the
   * pinned cursor is owned by `useDebuggerStore` instead (femtoseconds).
   */
  const [localPinnedTime, setLocalPinnedTime] = useState<number | null>(null);
  const pinnedTimeFs = useDebuggerStore((s) => s.pinnedTimeFs);
  const debuggerSessionId = useDebuggerStore((s) => s.sessionId);
  const seekDebugger = useDebuggerStore((s) => s.seek);

  /** VCD seconds-per-tick, memoized to avoid redundant parsing. */
  const secPerTick = useMemo(() => vcdSecondsPerTick(meta), [meta]);

  /** Convert store femtosecond time → VCD tick time for drawing. */
  const pinnedTimeTicks = useMemo(() => {
    if (pinnedTimeFs == null || secPerTick == null || !(secPerTick > 0)) return null;
    return pinnedTimeFs / 1e15 / secPerTick;
  }, [pinnedTimeFs, secPerTick]);

  /** Unified "pinned" (sticky) cursor in tick units. Prefers the backend-owned pin. */
  const pinnedTime = pinnedTimeTicks ?? localPinnedTime;

  /** What the values column shows: hover if hovering, else pinned. */
  const cursorTime = hoverTime ?? pinnedTime;

  /** Pin time at `t` (tick units): sync with the debugger backend if we can, else locally. */
  const pinAt = useCallback(
    (tTicks: number) => {
      if (debuggerSessionId != null && secPerTick != null && secPerTick > 0) {
        const fs = Math.round(tTicks * secPerTick * 1e15);
        void seekDebugger(fs);
      }
      setLocalPinnedTime(tTicks);
    },
    [debuggerSessionId, secPerTick, seekDebugger],
  );

  const viewMetaRef = useRef({ tView0, tView1, meta, selectedArr: [] as TraceKey[] });
  const navRef = useRef({ tView0, tView1, cursorTime, meta: null as VcdOpenResponse | null });
  const signalInfo = useMemo(() => (meta ? walkVars(meta) : new Map()), [meta]);

  const queryStateRef = useRef({
    meta: null as VcdOpenResponse | null,
    tView0: 0,
    tView1: 1,
    querySignalIds: [] as number[],
    projectRoot: "",
    vcdPath: "",
  });

  /** Coalesce rapid view-window changes (trackpad zoom/pan) to one query per animation frame. */
  const queryRafRef = useRef<number | null>(null);
  const queryGenRef = useRef(0);
  const flushQueryRef = useRef<() => void>(() => {});
  const onToastRef = useRef(onToast);
  onToastRef.current = onToast;

  useEffect(() => {
    let cancelled = false;
    const openSeq = allocWaveformVcdOpenSeq();
    setMeta(null);
    void (async () => {
      try {
        const res = await invoke<VcdOpenResponse>("vcd_open", {
          projectRoot,
          path: vcdPath,
          openSeq,
        });
        if (cancelled || res.superseded) {
          return;
        }
        const { tMin, tMax } = timeBounds(res);
        const first: number[] = [];
        const walk = (n: VcdScopeNode) => {
          for (const v of n.vars) {
            if (first.length < 8) first.push(v.signalId);
          }
          for (const s of n.scopes) walk(s);
        };
        for (const root of res.hierarchy) walk(root);
        const persisted = loadPersistedUiState(vcdPath);
        const available = availableTraceKeys(res);
        const defaultSelected = first.map(traceKeyWhole).sort(compareTraceKey);
        const selectedKeys =
          persisted && persisted.selected.length > 0
            ? persisted.selected.filter((k) => available.has(k)).sort(compareTraceKey)
            : defaultSelected;
        const qIds = [...first].sort((a, b) => a - b);
        // Refs must match the new file before the first `fireQuery`; otherwise a pending rAF can
        // still read the previous file's time window (see stale reqT1 vs ttMax in debug logs).
        queryStateRef.current = {
          ...queryStateRef.current,
          meta: res,
          tView0: tMin,
          tView1: tMax,
          querySignalIds: qIds,
          projectRoot,
          vcdPath,
        };
        viewMetaRef.current = {
          ...viewMetaRef.current,
          meta: res,
          tView0: tMin,
          tView1: tMax,
          selectedArr: selectedKeys,
        };
        setMeta(res);
        setCollapsedScopePaths(
          new Set((persisted?.collapsedScopePaths ?? []).filter((k) => typeof k === "string")),
        );
        setExpandedBusKeys(
          new Set((persisted?.expandedBusKeys ?? []).filter((k) => typeof k === "string")),
        );
        setTView0(tMin);
        setTView1(tMax);
        setFocusTraceKey(
          persisted?.focusTraceKey && available.has(persisted.focusTraceKey)
            ? persisted.focusTraceKey
            : null,
        );
        setSelected(new Set(selectedKeys.length > 0 ? selectedKeys : defaultSelected));
        queueMicrotask(() => flushQueryRef.current());
      } catch (e) {
        if (!cancelled) {
          onToastRef.current?.("error", e instanceof Error ? e.message : String(e));
        }
      }
    })();
    return () => {
      cancelled = true;
      void invoke("vcd_close").catch(() => {});
    };
  }, [projectRoot, vcdPath]);

  useEffect(() => {
    savePersistedUiState(vcdPath, {
      selected: Array.from(selected),
      focusTraceKey,
      expandedBusKeys: Array.from(expandedBusKeys),
      collapsedScopePaths: Array.from(collapsedScopePaths),
    });
  }, [vcdPath, selected, focusTraceKey, expandedBusKeys, collapsedScopePaths]);

  useEffect(() => {
    if (focusTraceKey != null && !selected.has(focusTraceKey)) {
      setFocusTraceKey(null);
    }
  }, [selected, focusTraceKey]);

  const selectedArr = useMemo(() => Array.from(selected).sort(compareTraceKey), [selected]);

  viewMetaRef.current = { tView0, tView1, meta, selectedArr };
  navRef.current = { tView0, tView1, cursorTime, meta };

  const querySignalIds = useMemo(() => {
    const ids = new Set<number>();
    for (const k of selectedArr) {
      ids.add(parseTraceKey(k).signalId);
    }
    if (focusTraceKey != null) ids.add(parseTraceKey(focusTraceKey).signalId);
    return Array.from(ids).sort((a, b) => a - b);
  }, [selectedArr, focusTraceKey]);

  queryStateRef.current = {
    meta,
    tView0,
    tView1,
    querySignalIds,
    projectRoot,
    vcdPath,
  };

  const fireQuery = useCallback(async () => {
    const st = queryStateRef.current;
    const gen = ++queryGenRef.current;
    if (!st.meta || st.querySignalIds.length === 0) {
      setTransitions([]);
      return;
    }
    const { tStart, tEnd } = queryTimeWindow(st.tView0, st.tView1, st.meta);
    try {
      const w = Math.max(32, Math.floor(plotCssWidth));
      const res = await invoke<{ transitions: VcdTransition[] }>("vcd_query", {
        projectRoot: st.projectRoot,
        path: st.vcdPath,
        signalIds: st.querySignalIds,
        tStart,
        tEnd,
        maxPointsPerSignal: Math.min(2_097_152, Math.max(w * 16, 65_536)),
        viewportWidthPx: w,
      });
      if (gen !== queryGenRef.current) return;
      setTransitions(res.transitions);
    } catch (e) {
      if (gen !== queryGenRef.current) return;
      onToast?.("error", e instanceof Error ? e.message : String(e));
    }
  }, [onToast, plotCssWidth]);

  const scheduleQueryRaf = useCallback(() => {
    if (queryRafRef.current != null) {
      cancelAnimationFrame(queryRafRef.current);
      queryRafRef.current = null;
    }
    queryRafRef.current = requestAnimationFrame(() => {
      queryRafRef.current = null;
      void fireQuery();
    });
  }, [fireQuery]);

  const flushQuery = useCallback(() => {
    if (queryRafRef.current != null) {
      cancelAnimationFrame(queryRafRef.current);
      queryRafRef.current = null;
    }
    void fireQuery();
  }, [fireQuery]);
  flushQueryRef.current = flushQuery;

  useEffect(() => {
    if (!meta) return;
    if (querySignalIds.length === 0) {
      setTransitions([]);
      return;
    }
    scheduleQueryRaf();
    return () => {
      if (queryRafRef.current != null) {
        cancelAnimationFrame(queryRafRef.current);
        queryRafRef.current = null;
      }
    };
  }, [meta, querySignalIds, tView0, tView1, projectRoot, vcdPath, plotCssWidth, scheduleQueryRaf]);

  const jumpToEdge = useCallback(
    async (next: boolean, edgeKind: string) => {
      const st = navRef.current;
      if (!st.meta) return;
      const arr = viewMetaRef.current.selectedArr;
      if (arr.length === 0) return;
      const trace = focusTraceKey ?? arr[0]!;
      const { signalId: sid, bit } = parseTraceKey(trace);
      const mid = (Math.min(st.tView0, st.tView1) + Math.max(st.tView0, st.tView1)) / 2;
      const from = st.cursorTime != null ? st.cursorTime : mid;
      try {
        const t = await invoke<number | null>("vcd_find_edge", {
          projectRoot,
          path: vcdPath,
          signalId: sid,
          fromTime: Math.max(0, Math.floor(from)),
          next,
          edgeKind,
          bitLsb: bit != null ? bit : null,
        });
        if (t == null) {
          onToast?.(
            "warning",
            `No ${edgeKind} edge ${next ? "after" : "before"} the sample time`,
          );
          return;
        }
        pinAt(t);
        const st2 = navRef.current;
        const m = st2.meta;
        if (!m) return;
        const a = Math.min(st2.tView0, st2.tView1);
        const b = Math.max(st2.tView0, st2.tView1);
        const span = Math.max(1, b - a);
        if (t < a || t > b) {
          const half = span / 2;
          const [na, nb] = clampViewToMeta(t - half, t + half, m);
          setTView0(na);
          setTView1(nb);
        }
        flushQuery();
      } catch (e) {
        onToast?.("error", e instanceof Error ? e.message : String(e));
      }
    },
    [focusTraceKey, projectRoot, vcdPath, flushQuery, onToast, pinAt],
  );

  const fitEntireTrace = useCallback(() => {
    const m = viewMetaRef.current.meta;
    if (!m) return;
    const { tMin, tMax } = timeBounds(m);
    setTView0(tMin);
    setTView1(tMax);
    const st = queryStateRef.current;
    queryStateRef.current = {
      ...st,
      meta: st.meta ?? m,
      tView0: tMin,
      tView1: tMax,
    };
    viewMetaRef.current = {
      ...viewMetaRef.current,
      tView0: tMin,
      tView1: tMax,
    };
    flushQuery();
  }, [flushQuery]);

  const applyZoom = useCallback((zoomFactor: number, anchorFrac: number) => {
    const m = viewMetaRef.current.meta;
    if (!m) return;
    const { tMin, tMax } = timeBounds(m);
    const v0 = viewMetaRef.current.tView0;
    const v1 = viewMetaRef.current.tView1;
    const t0 = Math.min(v0, v1);
    const t1 = Math.max(v0, v1);
    const span = Math.max(1, t1 - t0);
    const f = Math.min(1, Math.max(0, anchorFrac));
    const tAt = t0 + f * span;
    let nSpan = span * zoomFactor;
    const maxSpan = Math.max(1, tMax - tMin);
    nSpan = Math.max(1, Math.min(maxSpan, nSpan));
    const n0 = tAt - f * nSpan;
    const n1 = tAt + (1 - f) * nSpan;
    const [a, b] = clampViewToMeta(n0, n1, m);
    setTView0(a);
    setTView1(b);
  }, []);

  const toggleTrace = useCallback((key: TraceKey) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  const toggleBusExpand = useCallback((busKey: string) => {
    setExpandedBusKeys((prev) => {
      const next = new Set(prev);
      if (next.has(busKey)) next.delete(busKey);
      else next.add(busKey);
      return next;
    });
  }, []);

  const toggleScopeCollapse = useCallback((path: string) => {
    setCollapsedScopePaths((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const transitionsBySignal = useMemo(() => {
    const m = new Map<number, VcdTransition[]>();
    for (const tr of transitions) {
      const list = m.get(tr.signalId) ?? [];
      list.push(tr);
      m.set(tr.signalId, list);
    }
    for (const list of m.values()) {
      list.sort((a, b) => a.time - b.time);
    }
    return m;
  }, [transitions]);

  /** Snap the sample time to edges of focused trace, or the first visible row if none focused. */
  const snapTargetTraceKey = useMemo(
    () => focusTraceKey ?? (selectedArr.length > 0 ? selectedArr[0]! : null),
    [focusTraceKey, selectedArr],
  );

  const snapEdgeTimes = useMemo(() => {
    if (snapTargetTraceKey == null) return [];
    const list = transitionsForTraceKey(snapTargetTraceKey, transitionsBySignal, signalInfo);
    const times = list.map((t) => t.time).sort((a, b) => a - b);
    const out: number[] = [];
    for (const tm of times) {
      if (out.length === 0 || out[out.length - 1] !== tm) out.push(tm);
    }
    return out;
  }, [snapTargetTraceKey, transitionsBySignal, signalInfo]);

  const timescaleBanner = useMemo(() => {
    if (!meta) return null;
    const ts =
      meta.timescaleFactor != null && meta.timescaleUnit
        ? `${meta.timescaleFactor} ${meta.timescaleUnit}`
        : null;
    return { ts };
  }, [meta]);

  const redraw = useCallback(() => {
    const c = canvasRef.current;
    if (!c || !meta) return;
    const ctx = c.getContext("2d");
    if (!ctx) return;
    ctx.imageSmoothingEnabled = false;
    const rect = c.getBoundingClientRect();
    const w = rect.width;
    const h = rect.height;
    const [t0, t1] = clampViewToMeta(tView0, tView1, meta);
    const span = t1 - t0 || 1;
    const { times: tickTimes, step: tickStep } = buildRulerTicks(t0, t1, RULER_TICKS);

    const tToX = (t: number) => ((t - t0) / span) * w;

    ctx.fillStyle = COL_BG;
    ctx.fillRect(0, 0, w, h);

    ctx.fillStyle = COL_RULER_BG;
    ctx.fillRect(0, 0, w, RULER_H);
    ctx.strokeStyle = COL_ROW_STROKE;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, RULER_H - 0.5);
    ctx.lineTo(w, RULER_H - 0.5);
    ctx.stroke();

    const tickBottom = RULER_H - 0.5;
    const tickTop = RULER_H - 9;
    const labelY = RULER_H - 11;
    ctx.font = "10px ui-monospace, SFMono-Regular, Menlo, monospace";

    for (const txTime of tickTimes) {
      const x = tToX(txTime);
      if (x < -2 || x > w + 2) continue;
      const xs = Math.round(x) + 0.5;
      ctx.strokeStyle = COL_RULER_TICK;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(xs, tickTop);
      ctx.lineTo(xs, tickBottom);
      ctx.stroke();
      const label = formatRulerTickLabel(txTime, tickStep, meta, t0, t1);
      ctx.fillStyle = COL_RULER_TEXT;
      ctx.textBaseline = "bottom";
      const nearL = x < 40;
      const nearR = x > w - 40;
      if (nearL) {
        ctx.textAlign = "left";
        ctx.fillText(label, Math.max(3, x), labelY);
      } else if (nearR) {
        ctx.textAlign = "right";
        ctx.fillText(label, Math.min(w - 3, x), labelY);
      } else {
        ctx.textAlign = "center";
        ctx.fillText(label, x, labelY);
      }
    }
    ctx.textAlign = "left";
    ctx.textBaseline = "alphabetic";

    ctx.strokeStyle = COL_GRID;
    ctx.lineWidth = 1;
    for (const txTime of tickTimes) {
      const x = tToX(txTime);
      if (x < -2 || x > w + 2) continue;
      const xs = Math.round(x) + 0.5;
      ctx.beginPath();
      ctx.moveTo(xs, RULER_H);
      ctx.lineTo(xs, h);
      ctx.stroke();
    }

    const bySig = new Map<number, VcdTransition[]>();
    for (const tr of transitions) {
      const list = bySig.get(tr.signalId) ?? [];
      list.push(tr);
      bySig.set(tr.signalId, list);
    }
    for (const list of bySig.values()) {
      list.sort((a, b) => a.time - b.time);
    }

    let row = 0;
    for (const traceKey of selectedArr) {
      const { signalId: sid, bit } = parseTraceKey(traceKey);
      const y0 = RULER_H + row * ROW_H;
      const rowBg = row % 2 === 0 ? COL_ROW_BG : COL_ROW_ALT;
      const info = signalInfo.get(sid);
      const bits = info?.bits ?? 1;
      let list = bySig.get(sid) ?? [];
      if (bit != null) {
        list = projectTransitionsToBit(list, bit, Math.max(1, bits));
      }
      const sampleVal = list[0]?.value ?? "0";
      const asBus = bit == null && isMultiBitStyle(bits, sampleVal);

      ctx.fillStyle = rowBg;
      ctx.fillRect(0, y0, w, ROW_H - 1);

      ctx.strokeStyle = COL_ROW_STROKE;
      ctx.lineWidth = 1;
      ctx.strokeRect(0, y0, w, ROW_H - 1);

      if (list.length === 0) {
        row++;
        continue;
      }

      if (asBus) {
        const yPad = 7;
        const yHi = y0 + yPad;
        const yLo = y0 + ROW_H - yPad - 2;
        const yMid = (yHi + yLo) / 2;
        const segs = buildBusSegments(list, t0, t1, tToX, w);

        ctx.font = "600 11px ui-monospace, SFMono-Regular, Menlo, monospace";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";

        const busR = 4;
        const useRound =
          typeof (ctx as CanvasRenderingContext2D & { roundRect?: unknown }).roundRect ===
          "function";
        for (const seg of segs) {
          const sw = seg.x1 - seg.x0;
          if (sw < 1) continue;
          ctx.fillStyle = COL_BUS_FILL;
          ctx.beginPath();
          if (useRound) {
            ctx.roundRect(seg.x0, yHi, sw, yLo - yHi, busR);
          } else {
            ctx.rect(seg.x0, yHi, sw, yLo - yHi);
          }
          ctx.fill();

          ctx.strokeStyle = COL_BUS_STROKE;
          ctx.lineWidth = 1.15;
          ctx.beginPath();
          if (useRound) {
            ctx.roundRect(seg.x0 + 0.5, yHi + 0.5, sw - 1, yLo - yHi - 1, Math.max(1, busR - 1));
          } else {
            ctx.rect(seg.x0 + 0.5, yHi + 0.5, sw - 1, yLo - yHi - 1);
          }
          ctx.stroke();

          const label = formatValueDisplay(seg.val, busRadix);
          if (sw > 36) {
            ctx.fillStyle = COL_LABEL;
            const maxChars = Math.floor((sw - 8) / 7);
            const text =
              maxChars > 2 && label.length > maxChars ? `${label.slice(0, Math.max(1, maxChars - 1))}…` : label;
            ctx.fillText(text, seg.x0 + sw / 2, yMid);
          }
        }
        ctx.textAlign = "left";
        ctx.textBaseline = "alphabetic";
      } else {
        const yHi = y0 + ROW_H * 0.26;
        const yLo = y0 + ROW_H * 0.74;
        const first = list[0]!;
        let prevX = tToX(Math.min(t0, first.time));
        const firstLvl = parseBinLevel(first.value);
        let prevY = firstLvl === 1 ? yHi : firstLvl === 0 ? yLo : (yHi + yLo) / 2;

        ctx.strokeStyle = COL_WIRE;
        ctx.lineWidth = 1.85;
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.beginPath();
        ctx.moveTo(prevX, prevY);

        for (let i = 0; i < list.length; i++) {
          const tr = list[i]!;
          const x = tToX(tr.time);
          const lvl = parseBinLevel(tr.value);
          const y = lvl === 1 ? yHi : lvl === 0 ? yLo : (yHi + yLo) / 2;
          if (i === 0 && x > prevX) {
            ctx.lineTo(x, prevY);
          } else if (i > 0) {
            ctx.lineTo(x, prevY);
          }
          ctx.lineTo(x, y);
          prevX = x;
          prevY = y;
        }
        ctx.lineTo(w, prevY);
        ctx.stroke();
      }

      row++;
    }

    // Dashed hover ("ghost") cursor. Drawn first so the solid pin renders on top.
    if (hoverTime !== null && hoverTime !== pinnedTime && hoverTime >= t0 && hoverTime <= t1) {
      const hx = Math.round(tToX(hoverTime)) + 0.5;
      ctx.save();
      ctx.strokeStyle = COL_HOVER_LINE;
      ctx.lineWidth = 1;
      ctx.setLineDash([3, 3]);
      ctx.beginPath();
      ctx.moveTo(hx, RULER_H);
      ctx.lineTo(hx, h);
      ctx.stroke();
      ctx.restore();
    }

    // Solid pinned cursor (shared with the editor via the debugger store).
    if (pinnedTime !== null && pinnedTime >= t0 && pinnedTime <= t1) {
      const cx = Math.round(tToX(pinnedTime)) + 0.5;
      ctx.strokeStyle = COL_SAMPLE_LINE;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(cx, RULER_H);
      ctx.lineTo(cx, h);
      ctx.stroke();

      ctx.font = "10px ui-monospace, SFMono-Regular, Menlo, monospace";
      ctx.textBaseline = "top";
      ctx.textAlign = "left";
      const tag = formatSampleTimeLabelWithMeta(pinnedTime, tickStep, meta, t0, t1);
      const tw = ctx.measureText(tag).width;
      let lx = cx + 6;
      if (lx + tw > w - 4) lx = Math.max(4, cx - tw - 6);
      const padX = 3;
      const padY = 2;
      ctx.fillStyle = COL_RULER_BG;
      ctx.fillRect(lx - padX, RULER_SAMPLE_Y - padY, tw + padX * 2, 13);
      ctx.fillStyle = COL_SAMPLE_TAG;
      ctx.fillText(tag, lx, RULER_SAMPLE_Y);
      ctx.textBaseline = "alphabetic";
    }
  }, [
    meta,
    transitions,
    selectedArr,
    tView0,
    tView1,
    hoverTime,
    pinnedTime,
    signalInfo,
    busRadix,
  ]);

  useLayoutEffect(() => {
    redraw();
  }, [redraw]);

  const plotScrollRef = useRef<HTMLDivElement | null>(null);
  const canvasHostRef = useRef<HTMLDivElement | null>(null);
  const valuesPlotRef = useRef<HTMLDivElement | null>(null);

  const syncPlotLayout = useCallback(() => {
    const scrollEl = plotScrollRef.current;
    const host = canvasHostRef.current;
    const canvas = canvasRef.current;
    const vals = valuesPlotRef.current;
    if (!scrollEl || !host || !canvas) return;
    const dpr = window.devicePixelRatio || 1;
    const cw = Math.max(1, Math.floor(host.clientWidth));
    const vh = Math.max(1, Math.floor(scrollEl.clientHeight));
    const n = viewMetaRef.current.selectedArr.length;
    const contentH = Math.max(vh, RULER_H + n * ROW_H);
    if (vals) vals.style.minHeight = `${contentH}px`;
    canvas.style.width = `${cw}px`;
    canvas.style.height = `${contentH}px`;
    canvas.width = Math.max(1, Math.floor(cw * dpr));
    canvas.height = Math.max(1, Math.floor(contentH * dpr));
    const cx = canvas.getContext("2d");
    if (cx) {
      cx.setTransform(dpr, 0, 0, dpr, 0, 0);
      cx.imageSmoothingEnabled = false;
    }
    redraw();
  }, [redraw]);

  useLayoutEffect(() => {
    syncPlotLayout();
  }, [syncPlotLayout, selectedArr.length]);

  useLayoutEffect(() => {
    const scrollEl = plotScrollRef.current;
    const host = canvasHostRef.current;
    if (!scrollEl) return;
    const ro = new ResizeObserver(() => syncPlotLayout());
    ro.observe(scrollEl);
    if (host) ro.observe(host);
    return () => ro.disconnect();
  }, [syncPlotLayout]);

  useEffect(() => {
    const onResize = () => syncPlotLayout();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [syncPlotLayout]);

  const wheelHandlerRef = useRef<(e: WheelEvent) => void>(() => {});
  wheelHandlerRef.current = (e: WheelEvent) => {
    const m = viewMetaRef.current.meta;
    if (!m) return;
    e.preventDefault();
    const el = e.currentTarget as HTMLCanvasElement;
    const rect = el.getBoundingClientRect();
    const plotW = rect.width || 1;
    const { tView0: v0, tView1: v1 } = viewMetaRef.current;
    const [vt0, vt1] = clampViewToMeta(v0, v1, m);
    const span = Math.max(1, vt1 - vt0);
    const dx = e.deltaX;
    const dy = e.deltaY;
    if (Math.abs(dx) > Math.abs(dy) && Math.abs(dx) > 0.25) {
      const dt = ((dx * SWIPE_PAN_FACTOR) / plotW) * span;
      const raw0 = Math.min(v0, v1) + dt;
      const raw1 = Math.max(v0, v1) + dt;
      const [a, b] = clampViewToMeta(raw0, raw1, m);
      setTView0(a);
      setTView1(b);
      return;
    }
    const mx = e.clientX - rect.left;
    const frac = Math.min(1, Math.max(0, mx / plotW));
    const zoom = dy > 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
    applyZoom(zoom, frac);
  };

  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const w = (ev: WheelEvent) => wheelHandlerRef.current(ev);
    canvas.addEventListener("wheel", w, { passive: false });
    return () => canvas.removeEventListener("wheel", w);
  }, []);

  useEffect(() => {
    if (!meta) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "=" || e.key === "+") {
        e.preventDefault();
        applyZoom(1 / ZOOM_STEP, 0.5);
      } else if (e.key === "-" || e.key === "_") {
        e.preventDefault();
        applyZoom(ZOOM_STEP, 0.5);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [meta, applyZoom]);

  const endPointer = useCallback(() => {
    const p = pointerRef.current;
    if (!p.down) return;
    const wasPan = p.panning;
    if (!wasPan) {
      const c = canvasRef.current;
      const m = viewMetaRef.current.meta;
      const v0 = viewMetaRef.current.tView0;
      const v1 = viewMetaRef.current.tView1;
      if (c && m) {
        const rect = c.getBoundingClientRect();
        const mx = p.clientStartX - rect.left;
        if (mx >= 0 && mx <= rect.width) {
          const [t0, t1] = clampViewToMeta(v0, v1, m);
          const span = t1 - t0 || 1;
          const plotW = rect.width || 1;
          let tClick = t0 + (mx / plotW) * span;
          if (snapTargetTraceKey != null && snapEdgeTimes.length > 0) {
            let bestT = tClick;
            let bestDx = SNAP_PX + 1;
            for (const te of snapEdgeTimes) {
              const xe = ((te - t0) / span) * plotW;
              const d = Math.abs(xe - mx);
              if (d < bestDx) {
                bestDx = d;
                bestT = te;
              }
            }
            if (bestDx <= SNAP_PX) tClick = bestT;
          }
          pinAt(tClick);
        }
      }
    } else {
      flushQuery();
    }
    p.down = false;
    p.panning = false;
  }, [flushQuery, pinAt, snapTargetTraceKey, snapEdgeTimes]);

  useEffect(() => {
    window.addEventListener("mouseup", endPointer);
    return () => window.removeEventListener("mouseup", endPointer);
  }, [endPointer]);

  const onMouseDown = (e: React.MouseEvent) => {
    const c = canvasRef.current;
    if (!c) return;
    const rect = c.getBoundingClientRect();
    const my = e.clientY - rect.top;
    pointerRef.current = {
      down: true,
      clientStartX: e.clientX,
      clientStartY: e.clientY,
      canvasMy: my,
      panning: false,
      lastX: e.clientX,
    };
  };

  const onMouseMove = (e: React.MouseEvent) => {
    const c = canvasRef.current;
    if (!c) return;
    const { tView0: v0, tView1: v1, meta: m } = viewMetaRef.current;
    if (!m) return;
    const rect = c.getBoundingClientRect();
    const mx = e.clientX - rect.left;

    const p = pointerRef.current;
    if (p.down && !p.panning) {
      const dx = e.clientX - p.clientStartX;
      const dy = e.clientY - p.clientStartY;
      if (Math.hypot(dx, dy) >= PAN_THRESHOLD_PX) {
        p.panning = true;
      }
    }

    const [t0, t1] = clampViewToMeta(v0, v1, m);
    const span = t1 - t0 || 1;
    const plotW = rect.width || 1;
    let tHover = t0 + (mx / plotW) * span;

    if (snapTargetTraceKey != null && snapEdgeTimes.length > 0) {
      let bestT = tHover;
      let bestDx = SNAP_PX + 1;
      for (const te of snapEdgeTimes) {
        const xe = ((te - t0) / span) * plotW;
        const d = Math.abs(xe - mx);
        if (d < bestDx) {
          bestDx = d;
          bestT = te;
        }
      }
      if (bestDx <= SNAP_PX) tHover = bestT;
    }
    setHoverTime(tHover);

    if (p.down && p.panning) {
      // Ctrl/Cmd+drag in the ruler band: scrub the pinned cursor continuously.
      const inRuler = (e.clientY - rect.top) < RULER_H;
      if (inRuler) pinAt(tHover);
    }

    if (!p.down || !p.panning) return;
    const dx = e.clientX - p.lastX;
    p.lastX = e.clientX;
    const dt = (dx / plotW) * span;
    const raw0 = Math.min(v0, v1) + dt;
    const raw1 = Math.max(v0, v1) + dt;
    const [a, b] = clampViewToMeta(raw0, raw1, m);
    setTView0(a);
    setTView1(b);
  };

  const onMouseLeave = () => {
    const p = pointerRef.current;
    if (p.down && p.panning) flushQuery();
    p.down = false;
    p.panning = false;
    setHoverTime(null);
  };

  return (
    <div
      style={{
        flex: 1,
        display: "flex",
        flexDirection: "column",
        minHeight: 0,
        background: theme.shell.waveformChromeBg,
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          flexWrap: "wrap",
          gap: theme.space[2],
          padding: `${theme.space[1]}px ${theme.space[2]}px`,
          borderBottom: `1px solid ${theme.shell.sidebarBorder}`,
          fontSize: 12,
          color: theme.text.secondary,
        }}
      >
        <span style={{ overflow: "hidden", textOverflow: "ellipsis", minWidth: 120 }}>
          Waveform: {vcdPath.replace(/^.*[/\\]/, "")}
        </span>
        <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: 4 }}>
          {(
            [
              ["hex", Hash, "Hex bus values"] as const,
              ["bin", Binary, "Binary bus values"] as const,
              ["dec", Calculator, "Decimal bus values"] as const,
            ] as const
          ).map(([r, Icon, label]) => (
            <IconButton
              key={r}
              label={label}
              onClick={() => setBusRadix(r)}
              disabled={!meta}
              style={{
                background:
                  busRadix === r ? theme.waveform.accentSoft : undefined,
                border:
                  busRadix === r
                    ? `1px solid ${theme.accent.primary}`
                    : undefined,
                borderRadius: theme.radius.sm,
              }}
            >
              <Icon size={18} strokeWidth={1.75} />
            </IconButton>
          ))}
          <span
            style={{
              width: 1,
              height: 18,
              background: theme.shell.secondaryButtonBorder,
              margin: "0 4px",
            }}
          />
          <IconButton
            label="Zoom in (narrower time window)"
            onClick={() => applyZoom(1 / ZOOM_STEP, 0.5)}
            disabled={!meta}
          >
            <ZoomIn size={18} strokeWidth={1.75} />
          </IconButton>
          <IconButton
            label="Zoom out (wider time window)"
            onClick={() => applyZoom(ZOOM_STEP, 0.5)}
            disabled={!meta}
          >
            <ZoomOut size={18} strokeWidth={1.75} />
          </IconButton>
          <IconButton label="Fit entire trace (full simulation time range)" onClick={fitEntireTrace} disabled={!meta}>
            <Maximize2 size={18} strokeWidth={1.75} />
          </IconButton>
          <span
            style={{
              width: 1,
              height: 18,
              background: theme.shell.secondaryButtonBorder,
              margin: "0 4px",
            }}
          />
          {(
            [
              [ArrowBigLeft, false, "rising", "Previous rising edge"],
              [ArrowLeft, false, "falling", "Previous falling edge"],
              [Rewind, false, "any", "Previous any edge"],
              [FastForward, true, "any", "Next any edge"],
              [ArrowRight, true, "falling", "Next falling edge"],
              [ArrowBigRight, true, "rising", "Next rising edge"],
            ] as const
          ).map(([Icon, next, kind, label]) => (
            <IconButton
              key={`${label}-${next}-${kind}`}
              label={
                selectedArr.length === 0
                  ? "Add signals in the sidebar"
                  : label + " on focused row, else top visible row"
              }
              onClick={() => void jumpToEdge(next, kind)}
              disabled={!meta || selectedArr.length === 0}
            >
              <Icon size={17} strokeWidth={1.75} />
            </IconButton>
          ))}
        </div>
      </div>
      {timescaleBanner && meta ? (
        <div
          style={{
            display: "flex",
            flexWrap: "wrap",
            alignItems: "center",
            gap: 12,
            padding: "5px 10px",
            background: theme.waveform.rowBg,
            borderBottom: `1px solid ${theme.shell.sidebarBorder}`,
            fontSize: 11,
            color: theme.waveform.label,
          }}
        >
          <span>
            <span style={{ color: theme.text.muted, marginRight: 6 }}>File timescale</span>
            <strong style={{ color: theme.text.primary }}>
              {timescaleBanner.ts ?? "—"}
            </strong>
          </span>
          <span style={{ color: theme.waveform.mutedHint, fontSize: 10 }}>
            Waveforms are resampled to the plot width so the full range matches the pixels you see;
            ruler uses SI time from the window. Use “fit entire trace” if the view is zoomed in.
          </span>
        </div>
      ) : null}
      <div style={{ flex: 1, display: "flex", minHeight: 0 }}>
        <div
          style={{
            width: 220,
            flexShrink: 0,
            overflow: "auto",
            borderRight: `1px solid ${theme.shell.sidebarBorder}`,
            padding: 6,
          }}
        >
          {meta?.hierarchy.map((root, i) => (
            <ScopeTree
              key={`${root.name}-${i}`}
              node={root}
              depth={0}
              scopePath={`h${i}`}
              collapsedScopePaths={collapsedScopePaths}
              onToggleScopeCollapse={toggleScopeCollapse}
              expandedBusKeys={expandedBusKeys}
              onToggleBusExpand={toggleBusExpand}
              selected={selected}
              focusTraceKey={focusTraceKey}
              toggle={toggleTrace}
              signalInfo={signalInfo}
            />
          ))}
        </div>
        <div
          ref={plotScrollRef}
          style={{
            flex: 1,
            minWidth: 0,
            minHeight: 200,
            overflowY: "auto",
            overflowX: "hidden",
            display: "flex",
            flexDirection: "column",
          }}
        >
          <div style={{ display: "flex", flexDirection: "row", alignItems: "stretch", width: "100%" }}>
            <div
              ref={valuesPlotRef}
              style={{
                width: VALUES_COL_W,
                flexShrink: 0,
                borderRight: `1px solid ${theme.shell.sidebarBorder}`,
                background: theme.waveform.valuesColBg,
                boxSizing: "border-box",
              }}
            >
              <div
                style={{
                  height: RULER_H,
                  boxSizing: "border-box",
                  borderBottom: `1px solid ${COL_ROW_STROKE}`,
                  background: COL_RULER_BG,
                  display: "flex",
                  alignItems: "flex-end",
                  padding: "0 8px 4px",
                  fontSize: 9,
                  color: theme.text.muted,
                  fontWeight: 600,
                  letterSpacing: "0.04em",
                  textTransform: "uppercase",
                }}
              >
                Value
              </div>
              {selectedArr.length === 0 ? (
                <div
                  style={{
                    padding: 10,
                    color: theme.text.muted,
                    fontSize: 10,
                    lineHeight: 1.4,
                  }}
                >
                  Check signals in the scope list to see values here.
                </div>
              ) : (
                selectedArr.map((traceKey, row) => {
                  const name = traceDisplayName(traceKey, signalInfo);
                  const sorted = transitionsForTraceKey(traceKey, transitionsBySignal, signalInfo);
                  const raw = valueAtTime(sorted, cursorTime);
                  const display = raw == null ? "—" : formatValueDisplay(raw, busRadix);
                  const focused = focusTraceKey === traceKey;
                  const rowBg = row % 2 === 0 ? COL_ROW_BG : COL_ROW_ALT;
                  return (
                    <div
                      key={traceKey}
                      title={`${name} @ ${formatVcdTickForTooltip(cursorTime, meta, tView0, tView1)}`}
                      onClick={() =>
                        setFocusTraceKey((prev) => (prev === traceKey ? null : traceKey))
                      }
                      style={{
                        height: ROW_H,
                        boxSizing: "border-box",
                        padding: "5px 8px",
                        borderBottom: `1px solid ${COL_ROW_STROKE}`,
                        background: focused ? COL_FOCUS_VALUES_BG : rowBg,
                        borderLeft: focused ? `4px solid ${COL_FOCUS_BAR}` : "4px solid transparent",
                        display: "flex",
                        flexDirection: "column",
                        justifyContent: "center",
                        gap: 3,
                        cursor: "pointer",
                      }}
                    >
                      <div
                        style={{
                          fontSize: 10,
                          color: focused ? theme.waveform.scopeVarFocus : theme.text.secondary,
                          fontWeight: focused ? 600 : 500,
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                        }}
                      >
                        {focused ? "● " : ""}
                        {name}
                      </div>
                      <div
                        style={{
                          fontFamily: theme.font.mono,
                          fontSize: 11,
                          color: raw == null ? theme.waveform.mutedHint : theme.text.primary,
                          wordBreak: "break-all",
                          lineHeight: 1.25,
                        }}
                      >
                        {display}
                      </div>
                    </div>
                  );
                })
              )}
            </div>
            <div ref={canvasHostRef} style={{ flex: 1, minWidth: 0, minHeight: 0 }}>
              <canvas
                ref={canvasRef}
                style={{
                  display: "block",
                  cursor: "crosshair",
                }}
                onMouseDown={onMouseDown}
                onMouseMove={onMouseMove}
                onMouseLeave={onMouseLeave}
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
