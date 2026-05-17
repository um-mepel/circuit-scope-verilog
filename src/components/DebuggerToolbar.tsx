import { useCallback, useEffect } from "react";
import {
  Bug,
  ChevronsRight,
  Pause,
  Play,
  Redo2,
  Square,
  StepForward,
} from "lucide-react";
import { theme } from "../ui/theme";
import { IconButton } from "./IconButton";
import { useDebuggerStore } from "../state/debuggerStore";

/**
 * The small toolbar that drives the resumable simulator. Lives above the
 * waveform panel when a debug session is active. Mirrors the shortcuts
 * bound in `App.tsx` (F10, F8, F5, Shift+F5) so users can see which
 * action each key takes.
 */
export type DebuggerToolbarProps = {
  projectRoot: string;
  topModule?: string | null;
  numCycles?: number | null;
  /** Called after `sim_start` succeeds with the resulting VCD path. */
  onSessionStarted?: (vcdPath: string) => void;
  onToast?: (kind: "error" | "warning", message: string) => void;
};

function formatTimeFs(fs: number | null): string {
  if (fs == null) return "—";
  if (!Number.isFinite(fs)) return String(fs);
  // Pick a reasonable SI suffix so the toolbar stays compact.
  const abs = Math.abs(fs);
  if (abs >= 1e15) return `${(fs / 1e15).toFixed(2)} s`;
  if (abs >= 1e12) return `${(fs / 1e12).toFixed(2)} ms`;
  if (abs >= 1e9) return `${(fs / 1e9).toFixed(2)} \u00B5s`;
  if (abs >= 1e6) return `${(fs / 1e6).toFixed(2)} ns`;
  if (abs >= 1e3) return `${(fs / 1e3).toFixed(2)} ps`;
  return `${fs} fs`;
}

export function DebuggerToolbar({
  projectRoot,
  topModule,
  numCycles,
  onSessionStarted,
  onToast,
}: DebuggerToolbarProps) {
  const sessionId = useDebuggerStore((s) => s.sessionId);
  const mode = useDebuggerStore((s) => s.mode);
  const pinnedTimeFs = useDebuggerStore((s) => s.pinnedTimeFs);
  const pending = useDebuggerStore((s) => s.pending);
  const error = useDebuggerStore((s) => s.error);
  const start = useDebuggerStore((s) => s.start);
  const step = useDebuggerStore((s) => s.step);
  const stop = useDebuggerStore((s) => s.stop);
  const clearError = useDebuggerStore((s) => s.clearError);

  const active = sessionId != null;
  const disabled = pending || !active;

  const handleStart = useCallback(async () => {
    const res = await start({
      projectRoot,
      topModule: topModule ?? undefined,
      numCycles: numCycles ?? undefined,
    });
    if (res) onSessionStarted?.(res.vcdPath);
  }, [start, projectRoot, topModule, numCycles, onSessionStarted]);

  const handleStep = useCallback(
    (m: "statement" | "tick" | "cycle" | "run") => () => void step(m),
    [step],
  );

  const handleStop = useCallback(() => void stop(), [stop]);

  useEffect(() => {
    if (error) {
      onToast?.("error", error);
      clearError();
    }
  }, [error, onToast, clearError]);

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: theme.space[1],
        padding: `${theme.space[1]}px ${theme.space[2]}px`,
        borderBottom: `1px solid ${theme.shell.sidebarBorder}`,
        background: theme.shell.panelRaised,
        fontSize: 12,
        color: theme.text.secondary,
        flexShrink: 0,
      }}
    >
      <strong
        style={{
          marginRight: theme.space[2],
          display: "flex",
          alignItems: "center",
          gap: 6,
          fontSize: 11,
          textTransform: "uppercase",
          letterSpacing: "0.06em",
          color: theme.text.muted,
        }}
      >
        <Bug size={14} strokeWidth={1.75} /> Debug
      </strong>
      <IconButton
        label={active ? "Restart session" : "Start debug session (F5)"}
        onClick={() => void handleStart()}
        disabled={pending}
      >
        <Play size={16} strokeWidth={1.75} />
      </IconButton>
      <IconButton
        label="Step one statement (F10)"
        onClick={handleStep("statement")}
        disabled={disabled}
      >
        <StepForward size={16} strokeWidth={1.75} />
      </IconButton>
      <IconButton
        label="Step one tick (F11)"
        onClick={handleStep("tick")}
        disabled={disabled}
      >
        <Redo2 size={16} strokeWidth={1.75} />
      </IconButton>
      <IconButton
        label="Step one cycle (F8)"
        onClick={handleStep("cycle")}
        disabled={disabled}
      >
        <ChevronsRight size={16} strokeWidth={1.75} />
      </IconButton>
      <IconButton
        label="Run to end"
        onClick={handleStep("run")}
        disabled={disabled}
      >
        <Play size={16} strokeWidth={1.75} style={{ transform: "scaleX(1.3)" }} />
      </IconButton>
      <IconButton
        label="Pause (not yet supported)"
        onClick={() => {}}
        disabled={true}
      >
        <Pause size={16} strokeWidth={1.75} />
      </IconButton>
      <IconButton
        label="Stop debug session (Shift+F5)"
        onClick={handleStop}
        disabled={!active || pending}
      >
        <Square size={16} strokeWidth={1.75} />
      </IconButton>
      <span
        style={{
          marginLeft: theme.space[2],
          fontFamily: theme.font.mono,
          color: theme.text.primary,
        }}
      >
        t = {formatTimeFs(pinnedTimeFs)}
      </span>
      <span
        style={{
          marginLeft: theme.space[2],
          color:
            mode === "running"
              ? theme.accent.primary
              : mode === "paused"
                ? theme.text.secondary
                : theme.text.muted,
          fontSize: 11,
          textTransform: "uppercase",
          letterSpacing: "0.08em",
        }}
      >
        {mode}
      </span>
    </div>
  );
}
