import { useState, useEffect, useCallback, useRef } from "react";
import type React from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile, writeTextFile, readDir } from "@tauri-apps/plugin-fs";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { join } from "@tauri-apps/api/path";
import { APP_VERSION } from "./appVersion";
import { logAction } from "./logger";
import {
  Activity,
  Bug,
  FilePlus,
  FileText,
  FolderOpen,
  FolderPlus,
  Save,
  Terminal,
} from "lucide-react";
import { TerminalPane } from "./components/TerminalPane";
import { WaveformPanel } from "./components/WaveformPanel";
import { VerilogEditor } from "./components/VerilogEditor";
import { IconButton } from "./components/IconButton";
import { DebuggerToolbar } from "./components/DebuggerToolbar";
import { useToastStack } from "./components/ToastStack";
import { useDebuggerStore } from "./state/debuggerStore";
import { theme } from "./ui/theme";

const EDITOR_FONT_STORAGE_KEY = "circuitscope_editor_font_px";

type OpenedFile = {
  path: string;
  content: string;
};

type TreeEntry = {
  name: string;
  path: string;
  isDirectory: boolean;
  children?: TreeEntry[];
};

async function loadTree(dirPath: string): Promise<TreeEntry[]> {
  const entries = await readDir(dirPath);
  const result: TreeEntry[] = [];
  for (const entry of entries) {
    if (entry.name === ".DS_Store") {
      continue;
    }
    const path = await join(dirPath, entry.name);
    const node: TreeEntry = {
      name: entry.name,
      path,
      isDirectory: entry.isDirectory,
    };
    if (entry.isDirectory) {
      try {
        node.children = await loadTree(path);
        node.children.sort((a, b) => {
          if (a.isDirectory !== b.isDirectory)
            return a.isDirectory ? -1 : 1;
          return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
        });
      } catch {
        node.children = [];
      }
    }
    result.push(node);
  }
  result.sort((a, b) => {
    if (a.isDirectory !== b.isDirectory) return a.isDirectory ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
  });
  return result;
}

function FileTree({
  entries,
  onOpenFile,
  onMove,
  onContextMenu,
  renamePath,
  renameName,
  setRenameName,
  onRenameConfirm,
  onRenameCancel,
  rootPath,
}: {
  entries: TreeEntry[];
  onOpenFile: (path: string) => void;
  onMove: (fromPath: string, toFolderPath: string) => void;
  onContextMenu: (entry: TreeEntry, event: React.MouseEvent) => void;
  renamePath: string | null;
  renameName: string;
  setRenameName: (value: string) => void;
  onRenameConfirm: () => void;
  onRenameCancel: () => void;
  rootPath: string;
}) {
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const toggle = (path: string) => {
    setCollapsed((c) => ({ ...c, [path]: !c[path] }));
  };
  const handleDragStart = (path: string) => (ev: React.DragEvent) => {
    ev.dataTransfer.setData("text/plain", path);
    ev.dataTransfer.effectAllowed = "move";
  };
  const handleDropOnFolder = (folderPath: string) => (ev: React.DragEvent) => {
    ev.preventDefault();
    ev.stopPropagation();
    const from = ev.dataTransfer.getData("text/plain");
    if (!from) return;
    if (from === folderPath) return;
    const sep = folderPath.includes("\\") ? "\\" : "/";
    if (folderPath.startsWith(from + sep)) return;
    onMove(from, folderPath);
  };
  const handleDragOver = (ev: React.DragEvent) => {
    ev.preventDefault();
    ev.dataTransfer.dropEffect = "move";
  };
  return (
    <ul style={{ listStyle: "none", margin: 0, paddingLeft: "0.75rem" }}>
      {entries.map((e) => (
        <li key={e.path}>
          {e.isDirectory ? (
            <>
              <div
                draggable
                onDragStart={handleDragStart(e.path)}
                onDragOver={handleDragOver}
                onDrop={handleDropOnFolder(e.path)}
                style={{ display: "contents" }}
              >
                <button
                  type="button"
                  className="file-tree-row-dir"
                  onClick={() => toggle(e.path)}
                  onContextMenu={(ev) => {
                    ev.preventDefault();
                    onContextMenu(e, ev);
                  }}
                >
                  <span style={{ width: 14 }}>
                    {collapsed[e.path] ? "▶" : "▼"}
                  </span>
                  {e.path === renamePath ? (
                    <input
                      autoFocus
                      value={renameName}
                      onChange={(ev) => setRenameName(ev.target.value)}
                      onKeyDown={(ev) => {
                        if (ev.key === "Enter") {
                          ev.preventDefault();
                          onRenameConfirm();
                        } else if (ev.key === "Escape") {
                          ev.preventDefault();
                          onRenameCancel();
                        }
                      }}
                      onBlur={onRenameCancel}
                      style={{
                        flex: 1,
                        fontSize: "12px",
                        padding: "0 2px",
                        borderRadius: theme.radius.sm,
                        border: `1px solid ${theme.shell.inputBorder}`,
                        background: theme.shell.inputBg,
                        color: theme.text.primary,
                      }}
                    />
                  ) : (
                    <span>{e.name}</span>
                  )}
                </button>
              </div>
              {!collapsed[e.path] && e.children && e.children.length > 0 && (
                <FileTree
                  entries={e.children}
                  onOpenFile={onOpenFile}
                  onMove={onMove}
                  onContextMenu={onContextMenu}
                  renamePath={renamePath}
                  renameName={renameName}
                  setRenameName={setRenameName}
                  onRenameConfirm={onRenameConfirm}
                  onRenameCancel={onRenameCancel}
                  rootPath={rootPath}
                />
              )}
            </>
          ) : (
            <div
              draggable
              onDragStart={handleDragStart(e.path)}
              style={{ display: "contents" }}
            >
              <button
                type="button"
                className="file-tree-row-file"
                onClick={() => onOpenFile(e.path)}
                onContextMenu={(ev) => {
                  ev.preventDefault();
                  onContextMenu(e, ev);
                }}
              >
                {e.path === renamePath ? (
                  <input
                    autoFocus
                    value={renameName}
                    onChange={(ev) => setRenameName(ev.target.value)}
                    onKeyDown={(ev) => {
                      if (ev.key === "Enter") {
                        ev.preventDefault();
                        onRenameConfirm();
                      } else if (ev.key === "Escape") {
                        ev.preventDefault();
                        onRenameCancel();
                      }
                    }}
                    onBlur={onRenameCancel}
                    style={{
                      width: "100%",
                      fontSize: "12px",
                      padding: "0 2px",
                      borderRadius: theme.radius.sm,
                      border: `1px solid ${theme.shell.inputBorder}`,
                      background: theme.shell.inputBg,
                      color: theme.text.primary,
                    }}
                  />
                ) : (
                  e.name
                )}
              </button>
            </div>
          )}
        </li>
      ))}
    </ul>
  );
}

export default function App() {
  const [projectRoot, setProjectRoot] = useState<string | null>(null);
  const [tree, setTree] = useState<TreeEntry[]>([]);
  const [file, setFile] = useState<OpenedFile | null>(null);
  const [isDirty, setIsDirty] = useState(false);
  const { pushToast, toastStack } = useToastStack();

  const [recentFolders, setRecentFolders] = useState<string[]>([]);
  const [terminalVisible, setTerminalVisible] = useState(false);
  const terminalVisibleRef = useRef(false);
  const [terminalInstanceId, setTerminalInstanceId] = useState(0);
  const [waveformVisible, setWaveformVisible] = useState(false);
  const [waveformPath, setWaveformPath] = useState<string | null>(null);
  /** Bumped after each Generate VCD so WaveformPanel remounts when `vcdPath` is unchanged (same output file overwritten). */
  const [waveformMountKey, setWaveformMountKey] = useState(0);
  const [newRootEntryType, setNewRootEntryType] = useState<"file" | "folder" | null>(null);
  const [newRootEntryName, setNewRootEntryName] = useState("");
  const [renamePath, setRenamePath] = useState<string | null>(null);
  const [renameName, setRenameName] = useState("");
  const [contextMenu, setContextMenu] = useState<{
    path: string;
    isDirectory: boolean;
    x: number;
    y: number;
  } | null>(null);

  useEffect(() => {
    terminalVisibleRef.current = terminalVisible;
  }, [terminalVisible]);

  useEffect(() => {
    void invoke<{ verilogCoreVersion: string }>("compiler_info").then(
      (info) => {
        console.info(
          "[CircuitScope] Embedded verilog-core version:",
          info.verilogCoreVersion
        );
      },
      () => {
        console.warn("[CircuitScope] compiler_info unavailable (rebuild app?)");
      }
    );
  }, []);

  // Load recent folders from localStorage on first mount.
  useEffect(() => {
    try {
      const raw = window.localStorage.getItem("circuitscope_recent_folders");
      if (!raw) return;
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        setRecentFolders(parsed.filter((p) => typeof p === "string"));
      }
    } catch {
      // ignore parse errors
    }
  }, []);

  const rememberFolder = useCallback((folder: string) => {
    setRecentFolders((prev) => {
      const next = [folder, ...prev.filter((p) => p !== folder)];
      const limited = next.slice(0, 10);
      try {
        window.localStorage.setItem(
          "circuitscope_recent_folders",
          JSON.stringify(limited)
        );
      } catch {
        // ignore storage errors
      }
      return limited;
    });
  }, []);

  const setWarningForPath = useCallback(
    (path: string) => {
      let ext = "";
      const lastSlash = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
      const lastDot = path.lastIndexOf(".");
      if (lastDot > lastSlash) {
        ext = path.slice(lastDot + 1).toLowerCase();
      }
      if (ext && ext !== "v" && ext !== "sv" && ext !== "vcd") {
        pushToast(
          "warning",
          `Opened ".${ext}". Use .v / .sv for Verilog editing; .vcd opens in the waveform viewer; other types are plain text.`,
        );
      }
    },
    [pushToast],
  );

  const refreshTree = useCallback(async () => {
    if (!projectRoot) return;
    try {
      const entries = await loadTree(projectRoot);
      setTree(entries);
    } catch {
      // ignore
    }
  }, [projectRoot]);

  const openFolderAt = useCallback(
    async (folder: string) => {
      try {
        setProjectRoot(folder);
        const entries = await loadTree(folder);
        setTree(entries);
        setFile(null);
        setWaveformVisible(false);
        setWaveformPath(null);
        void invoke("vcd_close").catch(() => {});
        rememberFolder(folder);

        // Kick off backend indexing (non-blocking) and log results for now.
        try {
          const index = await invoke<{
            modules: { name: string; path: string }[];
          }>("index_project", { root: folder });
          console.log("index_project result", index);
          void logAction("index_project_success", {
            root: folder,
            moduleCount: index.modules.length,
          });
        } catch (e) {
          console.warn("index_project failed", e);
          void logAction("index_project_error", {
            root: folder,
            error: e instanceof Error ? e.message : String(e),
          });
        }
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        pushToast("error", `Could not open folder: ${message}`);
        void logAction("open_folder_error", {
          folder,
          error: message,
        });
      }
    },
    [rememberFolder, pushToast]
  );

  const handleOpenFolder = useCallback(async () => {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
      });
      if (!selected || Array.isArray(selected)) return;
      void logAction("menu_open_folder", { selected });
      await openFolderAt(selected);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", `Could not open folder: ${message}`);
      void logAction("menu_open_folder_error", { error: message });
    }
  }, [openFolderAt, pushToast]);

  const handleOpenFile = useCallback(async () => {
    try {
      const selected = await open({
        multiple: false,
      });
      if (!selected || Array.isArray(selected)) return;
      void logAction("open_file", { path: selected });
      const lastSlash = Math.max(selected.lastIndexOf("/"), selected.lastIndexOf("\\"));
      const lastDot = selected.lastIndexOf(".");
      const ext =
        lastDot > lastSlash ? selected.slice(lastDot + 1).toLowerCase() : "";
      if (ext === "vcd") {
        if (!projectRoot) {
          pushToast("error", "Open a project folder first to view waveforms.");
          return;
        }
        setWaveformPath(selected);
        setWaveformVisible(true);
        void logAction("open_vcd_waveform", { path: selected });
        return;
      }
      setWarningForPath(selected);
      const content = await readTextFile(selected);
      setFile({ path: selected, content });
      setIsDirty(false);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", `Could not open file: ${message}`);
      void logAction("open_file_error", { error: message });
    }
  }, [setWarningForPath, projectRoot, pushToast]);

  const openFileByPath = useCallback(
    async (path: string) => {
      try {
        const lastSlash = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
        const lastDot = path.lastIndexOf(".");
        const ext =
          lastDot > lastSlash ? path.slice(lastDot + 1).toLowerCase() : "";

        if (ext === "vcd") {
          if (!projectRoot) {
            pushToast("error", "Open a project folder first to view waveforms.");
            return;
          }
          setWaveformPath(path);
          setWaveformVisible(true);
          void logAction("open_vcd_waveform", { path });
          return;
        }

        setWarningForPath(path);

        setWaveformVisible(false);
        setWaveformPath(null);
        void invoke("vcd_close").catch(() => {});

        const content = await readTextFile(path);
        setFile({ path, content });
        setIsDirty(false);
        void logAction("open_file_from_tree", { path });

        // Verilog (1364) parser: only invoke on .v / .sv; can panic on unrelated text (e.g. VCD).
        if (ext === "v" || ext === "sv") {
          try {
            const result = await invoke<{
              diagnostics: { message: string; severity: string; line: number; column: number }[];
              modules: { name: string; ports: { direction?: string | null; name: string }[]; path: string }[];
            }>("parse_file", { path });
            console.log("parse_file result", result);
            void logAction("parse_file_success", {
              path,
              diagnosticCount: result.diagnostics.length,
              moduleCount: result.modules.length,
            });
          } catch (e) {
            console.warn("parse_file failed", e);
            void logAction("parse_file_error", {
              path,
              error: e instanceof Error ? e.message : String(e),
            });
          }
        }
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        pushToast("error", `Could not open file: ${message}`);
        void logAction("open_file_from_tree_error", { path, error: message });
      }
    },
    [setWarningForPath, projectRoot, pushToast]
  );

  const debuggerSessionId = useDebuggerStore((s) => s.sessionId);
  const debuggerVcdPath = useDebuggerStore((s) => s.vcdPath);
  const debuggerVcdTick = useDebuggerStore((s) => s.vcdTick);
  const startDebuggerSession = useDebuggerStore((s) => s.start);
  const stepDebugger = useDebuggerStore((s) => s.step);
  const stopDebugger = useDebuggerStore((s) => s.stop);
  const lastBreakpointHit = useDebuggerStore((s) => s.lastHit);
  const clearLastBreakpointHit = useDebuggerStore((s) => s.clearLastHit);

  // Surface a toast + open the hit file when a breakpoint fires during a
  // step. We clear the store's `lastHit` right after so a subsequent step
  // that lands on the *same* breakpoint still re-runs this effect.
  useEffect(() => {
    if (!lastBreakpointHit) return;
    const { path, line } = lastBreakpointHit;
    pushToast("warning", `Breakpoint hit at ${path}:${line}`);
    void openFileByPath(path);
    clearLastBreakpointHit();
  }, [lastBreakpointHit, pushToast, openFileByPath, clearLastBreakpointHit]);

  /** Start a resumable debug session and auto-open the waveform panel. */
  const handleStartDebug = useCallback(async () => {
    if (!projectRoot) {
      pushToast("error", "Open a folder first, then start a debug session.");
      return;
    }
    const res = await startDebuggerSession({
      projectRoot,
      vcdFilename: "debug.vcd",
    });
    if (res) {
      setWaveformPath(res.vcdPath);
      setWaveformVisible(true);
      setWaveformMountKey((k) => k + 1);
      void logAction("debug_session_started", {
        sessionId: res.sessionId,
        vcdPath: res.vcdPath,
      });
    }
  }, [projectRoot, startDebuggerSession, pushToast]);

  /** Remount waveform panel every time the debugger flushes a new VCD. */
  useEffect(() => {
    if (debuggerSessionId == null) return;
    if (!debuggerVcdPath) return;
    setWaveformPath(debuggerVcdPath);
    setWaveformVisible(true);
    setWaveformMountKey((k) => k + 1);
  }, [debuggerVcdTick, debuggerSessionId, debuggerVcdPath]);

  /**
   * When the debug session ends, drop the waveform panel so the (now deleted)
   * `debug.vcd` doesn't remain as a ghost entry in the viewer. Runs only on
   * the `active -> null` transition so it doesn't interfere with users who
   * want to view a freshly generated non-debug VCD.
   */
  const wasDebugActiveRef = useRef(false);
  useEffect(() => {
    const active = debuggerSessionId != null;
    if (wasDebugActiveRef.current && !active) {
      setWaveformVisible(false);
      setWaveformPath(null);
      void invoke("vcd_close").catch(() => {});
    }
    wasDebugActiveRef.current = active;
  }, [debuggerSessionId]);

  /** File → Generate VCD — `simulate_vcd` runs the same pipeline as the `csverilog` CLI in-process (no `cargo` subprocess). */
  const handleGenerateVcd = useCallback(async () => {
    if (!projectRoot) {
      pushToast("error", "Open a folder first, then generate a VCD.");
      void logAction("simulate_vcd_skipped", { reason: "no_project_root" });
      return;
    }
    try {
      const outPath = await invoke<string>("simulate_vcd", {
        root: projectRoot,
        outputFile: "circuit_scope.vcd",
      });
      console.info("[CircuitScope] Wrote VCD:", outPath);
      void logAction("simulate_vcd_success", { outPath });
      await refreshTree();
      setWaveformPath(outPath);
      setWaveformVisible(true);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", `Generate VCD failed: ${message}`);
      console.warn("simulate_vcd failed", e);
      void logAction("simulate_vcd_error", { error: message });
    }
  }, [projectRoot, refreshTree, pushToast]);

  const handleSave = useCallback(async () => {
    if (!file) return;
    try {
      await writeTextFile(file.path, file.content);
      setIsDirty(false);
      void logAction("save_file", { path: file.path });
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", `Could not save file: ${message}`);
      void logAction("save_file_error", {
        path: file.path,
        error: message,
      });
    }
  }, [file, pushToast]);

  const handleSaveRef = useRef(handleSave);
  handleSaveRef.current = handleSave;

  const [editorFontSizePx, setEditorFontSizePx] = useState(() => {
    try {
      const raw = window.localStorage.getItem(EDITOR_FONT_STORAGE_KEY);
      if (raw == null) return 14;
      const n = Number.parseInt(raw, 10);
      if (Number.isFinite(n) && n >= 10 && n <= 28) return n;
    } catch {
      /* ignore */
    }
    return 14;
  });

  useEffect(() => {
    try {
      window.localStorage.setItem(
        EDITOR_FONT_STORAGE_KEY,
        String(editorFontSizePx),
      );
    } catch {
      /* ignore */
    }
  }, [editorFontSizePx]);

  /** Cmd/Ctrl+S save, ± editor font/line height; never intercept copy/paste in editor, terminal, or inputs. */
  useEffect(() => {
    const mod = (e: KeyboardEvent) =>
      (e.metaKey || e.ctrlKey) && !e.altKey;

    /** CodeMirror often sets `target` to a Text node — must walk up to an Element. */
    const contextEl = (e: KeyboardEvent): Element | null => {
      const t = e.target;
      if (t instanceof Element) return t;
      if (t instanceof Text) return t.parentElement;
      return document.activeElement instanceof Element
        ? document.activeElement
        : null;
    };

    const inXterm = (e: KeyboardEvent) =>
      contextEl(e)?.closest(".xterm") != null;

    const useNativeClipboard = (e: KeyboardEvent) => {
      const el = contextEl(e);
      if (!el) return false;
      if (el.closest(".cm-editor") != null || el.closest(".xterm") != null)
        return true;
      const h = el as HTMLElement;
      if (h.tagName === "INPUT" || h.tagName === "TEXTAREA") return true;
      if (h.isContentEditable) return true;
      return false;
    };

    const onKeyDown = (e: KeyboardEvent) => {
      if (!mod(e)) return;

      const key = e.key;
      if (
        useNativeClipboard(e) &&
        (key === "c" ||
          key === "C" ||
          key === "v" ||
          key === "V" ||
          key === "x" ||
          key === "X") &&
        !e.shiftKey
      ) {
        return;
      }

      if ((key === "s" || key === "S") && !e.shiftKey) {
        e.preventDefault();
        void handleSaveRef.current();
        return;
      }

      if (inXterm(e)) return;

      const zoomIn =
        key === "+" || key === "=" || e.code === "NumpadAdd";
      const zoomOut =
        key === "-" || key === "_" || e.code === "NumpadSubtract";
      if (zoomIn || zoomOut) {
        e.preventDefault();
        setEditorFontSizePx((n) =>
          Math.min(28, Math.max(10, n + (zoomIn ? 1 : -1))),
        );
      }
    };

    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!projectRoot) return;
      const isShift = e.shiftKey;
      switch (e.key) {
        case "F5":
          e.preventDefault();
          if (isShift) {
            void stopDebugger();
          } else if (debuggerSessionId == null) {
            void handleStartDebug();
          } else {
            void stepDebugger("run");
          }
          return;
        case "F8":
          if (debuggerSessionId == null) return;
          e.preventDefault();
          void stepDebugger("cycle");
          return;
        case "F10":
          if (debuggerSessionId == null) return;
          e.preventDefault();
          void stepDebugger("statement");
          return;
        case "F11":
          if (debuggerSessionId == null) return;
          e.preventDefault();
          void stepDebugger("tick");
          return;
        default:
          return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [projectRoot, debuggerSessionId, handleStartDebug, stepDebugger, stopDebugger]);

  /** Menu / shortcut: open terminal only if closed (does not toggle). */
  const handleOpenNewTerminal = useCallback(() => {
    if (!projectRoot) return;
    if (terminalVisibleRef.current) return;
    setTerminalInstanceId((id) => id + 1);
    setTerminalVisible(true);
    void logAction("open_new_integrated_terminal", {
      projectRoot,
    });
  }, [projectRoot]);

  /** Toolbar: show or hide the docked terminal. */
  const handleToggleTerminal = useCallback(() => {
    if (!projectRoot) return;
    if (terminalVisibleRef.current) {
      setTerminalVisible(false);
      void logAction("close_integrated_terminal", {
        projectRoot,
      });
    } else {
      setTerminalInstanceId((id) => id + 1);
      setTerminalVisible(true);
      void logAction("open_new_integrated_terminal", {
        projectRoot,
      });
    }
  }, [projectRoot]);

  const handleCloseTerminal = useCallback(() => {
    if (!terminalVisible) return;
    setTerminalVisible(false);
    void logAction("close_integrated_terminal", {
      projectRoot,
    });
  }, [terminalVisible, projectRoot]);

  const handleNewFile = useCallback(() => {
    if (!projectRoot) return;
    setNewRootEntryType("file");
    setNewRootEntryName("");
  }, [projectRoot]);

  const handleNewFolder = useCallback(() => {
    if (!projectRoot) return;
    setNewRootEntryType("folder");
    setNewRootEntryName("");
  }, [projectRoot]);

  const cancelNewRootEntry = useCallback(() => {
    setNewRootEntryType(null);
    setNewRootEntryName("");
  }, []);

  const confirmNewRootEntry = useCallback(async () => {
    if (!projectRoot || !newRootEntryType) return;
    const name = newRootEntryName.trim();
    if (!name) {
      cancelNewRootEntry();
      return;
    }
    try {
      if (newRootEntryType === "file") {
        await invoke("create_file", {
          parentDir: projectRoot,
          name,
        });
        await refreshTree();
        const path = await join(projectRoot, name);
        await openFileByPath(path);
        void logAction("create_file", { path });
      } else {
        await invoke("create_dir", {
          parentDir: projectRoot,
          name,
        });
        await refreshTree();
        const path = await join(projectRoot, name);
        void logAction("create_dir", { path });
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", message);
    } finally {
      cancelNewRootEntry();
    }
  }, [projectRoot, newRootEntryType, newRootEntryName, refreshTree, openFileByPath, cancelNewRootEntry, pushToast]);

  const startRename = useCallback(
    (targetPath: string) => {
      setContextMenu(null);
      setNewRootEntryType(null);
      const base = targetPath.replace(/^.*[/\\]/, "");
      setRenamePath(targetPath);
      setRenameName(base);
    },
    []
  );

  const cancelRename = useCallback(() => {
    setRenamePath(null);
    setRenameName("");
  }, []);

  const confirmRename = useCallback(async () => {
    if (!renamePath || !projectRoot) return;
    const newName = renameName.trim();
    if (!newName) {
      cancelRename();
      return;
    }
    const lastSlash = Math.max(
      renamePath.lastIndexOf("/"),
      renamePath.lastIndexOf("\\")
    );
    const parent =
      lastSlash > 0 ? renamePath.slice(0, lastSlash) : projectRoot;
    try {
      const toPath = await join(parent, newName);
      await invoke("move_path", { from: renamePath, to: toPath });
      await refreshTree();
      if (file && file.path === renamePath) {
        setFile({ path: toPath, content: file.content });
      }
      void logAction("rename_path", { from: renamePath, to: toPath });
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      pushToast("error", message);
    } finally {
      cancelRename();
    }
  }, [renamePath, renameName, projectRoot, refreshTree, file, cancelRename, pushToast]);

  const handleDeletePath = useCallback(
    async (targetPath: string) => {
      setContextMenu(null);
      try {
        await invoke("delete_path", { path: targetPath });
        await refreshTree();
        if (file && file.path === targetPath) {
          setFile(null);
          setIsDirty(false);
        }
        void logAction("delete_path", { path: targetPath });
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        pushToast("error", message);
      }
    },
    [file, refreshTree, pushToast]
  );

  const handleEntryContextMenu = useCallback(
    (entry: TreeEntry, event: React.MouseEvent) => {
      setContextMenu({
        path: entry.path,
        isDirectory: entry.isDirectory,
        x: event.clientX,
        y: event.clientY,
      });
    },
    []
  );

  const handleMove = useCallback(
    async (fromPath: string, toFolderPath: string) => {
      const base = fromPath.replace(/^.*[/\\]/, "");
      const toPath = await join(toFolderPath, base);
      try {
        await invoke("move_path", { from: fromPath, to: toPath });
        await refreshTree();
        void logAction("move_path", { from: fromPath, to: toPath });
      } catch (e) {
        pushToast("error", e instanceof Error ? e.message : String(e));
      }
    },
    [refreshTree, pushToast]
  );

  useEffect(() => {
    const unsubs: (() => void)[] = [];
    listen("menu-open-folder", () => {
      handleOpenFolder();
    }).then((u) => unsubs.push(u));
    listen("menu-open-file", () => {
      handleOpenFile();
    }).then((u) => unsubs.push(u));
    listen("menu-save", () => {
      handleSave();
    }).then((u) => unsubs.push(u));
    listen("menu-open-new-terminal", () => {
      handleOpenNewTerminal();
    }).then((u) => unsubs.push(u));
    listen("menu-close-terminal", () => {
      handleCloseTerminal();
    }).then((u) => unsubs.push(u));
    listen("menu-generate-vcd", () => {
      void handleGenerateVcd();
    }).then((u) => unsubs.push(u));
    listen("menu-close-waveform", () => {
      setWaveformVisible(false);
      setWaveformPath(null);
      void invoke("vcd_close").catch(() => {});
    }).then((u) => unsubs.push(u));
    return () => {
      unsubs.forEach((fn) => fn());
    };
  }, [
    handleOpenFolder,
    handleOpenFile,
    handleSave,
    handleOpenNewTerminal,
    handleCloseTerminal,
    handleGenerateVcd,
  ]);

  // Refresh tree periodically so terminal-created/deleted files show up
  useEffect(() => {
    if (!projectRoot) return;
    const id = setInterval(refreshTree, 5000);
    return () => clearInterval(id);
  }, [projectRoot, refreshTree]);

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100vh" }}>
      <header
        style={{
          display: "flex",
          alignItems: "center",
          gap: `${theme.space[2]}px`,
          padding: `${theme.space[2]}px ${theme.space[3]}px`,
          borderBottom: `1px solid ${theme.shell.headerBorder}`,
          background: theme.shell.headerBg,
          color: theme.text.primary,
          boxShadow: `0 1px 0 ${theme.shell.panelBorder}`,
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: `${theme.space[1]}px`,
          }}
        >
          <strong
            style={{
              fontSize: "15px",
              fontWeight: 650,
              letterSpacing: "var(--cs-header-title-tracking)",
              marginRight: theme.space[2],
            }}
          >
            Circuit Scope
          </strong>
          <IconButton label="Open folder" onClick={() => void handleOpenFolder()}>
            <FolderOpen size={20} strokeWidth={1.75} />
          </IconButton>
          <IconButton label="Open file" onClick={() => void handleOpenFile()}>
            <FileText size={20} strokeWidth={1.75} />
          </IconButton>
          <IconButton
            label="Save"
            onClick={() => void handleSave()}
            disabled={!file || !isDirty}
          >
            <Save size={20} strokeWidth={1.75} />
          </IconButton>
          <IconButton
            label={
              !projectRoot
                ? "Open a folder first to use the terminal"
                : terminalVisible
                  ? "Hide integrated terminal"
                  : "Show integrated terminal"
            }
            onClick={handleToggleTerminal}
            disabled={!projectRoot}
            aria-pressed={terminalVisible}
          >
            <Terminal size={20} strokeWidth={1.75} />
          </IconButton>
          <IconButton
            label="Generate VCD"
            onClick={() => void handleGenerateVcd()}
            disabled={!projectRoot}
          >
            <Activity size={20} strokeWidth={1.75} />
          </IconButton>
          <IconButton
            label={
              debuggerSessionId == null
                ? "Start debug session (F5)"
                : "Restart debug session"
            }
            onClick={() => void handleStartDebug()}
            disabled={!projectRoot}
            aria-pressed={debuggerSessionId != null}
          >
            <Bug size={20} strokeWidth={1.75} />
          </IconButton>
        </div>
        <span
          style={{
            marginLeft: "auto",
            color: theme.text.headerMeta,
            fontSize: "0.82rem",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
            maxWidth: "min(52vw, 560px)",
          }}
          title={file ? file.path : projectRoot ?? undefined}
        >
          {file ? file.path : projectRoot ?? "No folder open"}
          {file && isDirty ? " *" : ""}
        </span>
      </header>
      <div
        style={{
          flex: 1,
          minHeight: 0,
          display: "flex",
          overflow: "hidden",
        }}
      >
        {projectRoot && (
          <aside
            style={{
              width: 260,
              minWidth: 200,
              borderRight: `1px solid ${theme.shell.sidebarBorder}`,
              background: theme.shell.sidebarBg,
              color: theme.text.secondary,
              overflow: "auto",
              padding: `${theme.space[2]}px 0`,
            }}
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
            }}
            onDrop={(e) => {
              e.preventDefault();
              const from = e.dataTransfer.getData("text/plain");
              if (from) handleMove(from, projectRoot);
            }}
          >
            <div
              className="cs-sidebar-section-header"
              style={{
                padding: `${theme.space[1]}px ${theme.space[2]}px`,
                paddingLeft: theme.space[2],
                fontSize: "11px",
                textTransform: "uppercase",
                letterSpacing: "0.06em",
                color: theme.text.muted,
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: `${theme.space[1]}px`,
              }}
            >
              <span>
                {projectRoot
                  ? projectRoot.replace(/^.*[/\\]/, "") || "Folder"
                  : "Folder"}
              </span>
              <div style={{ display: "flex", gap: `${theme.space[1] / 4}px` }}>
                <button
                  type="button"
                  className="cs-toolbar-icon"
                  title="New file in root"
                  onClick={handleNewFile}
                >
                  <FilePlus size={20} strokeWidth={1.75} aria-hidden />
                </button>
                <button
                  type="button"
                  className="cs-toolbar-icon"
                  title="New folder in root"
                  onClick={handleNewFolder}
                >
                  <FolderPlus size={20} strokeWidth={1.75} aria-hidden />
                </button>
              </div>
            </div>
            {newRootEntryType && (
              <div style={{ padding: `2px 6px 4px 14px` }}>
                <input
                  autoFocus
                  placeholder={
                    newRootEntryType === "file" ? "New file name" : "New folder name"
                  }
                  value={newRootEntryName}
                  onChange={(e) => setNewRootEntryName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void confirmNewRootEntry();
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      cancelNewRootEntry();
                    }
                  }}
                  onBlur={cancelNewRootEntry}
                  style={{
                    width: "100%",
                    fontSize: "12px",
                    padding: `${theme.space[1] / 4}px ${theme.space[1] / 2}px`,
                    borderRadius: theme.radius.sm,
                    border: `1px solid ${theme.shell.inputBorder}`,
                    background: theme.shell.inputBg,
                    color: theme.text.primary,
                  }}
                />
              </div>
            )}
            <FileTree
              entries={tree}
              onOpenFile={openFileByPath}
              onMove={handleMove}
               onContextMenu={handleEntryContextMenu}
               renamePath={renamePath}
               renameName={renameName}
               setRenameName={setRenameName}
               onRenameConfirm={confirmRename}
               onRenameCancel={cancelRename}
              rootPath={projectRoot}
            />
          </aside>
        )}
        <main
          style={{
            flex: 1,
            minHeight: 0,
            minWidth: 0,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
            background: theme.shell.appBg,
          }}
        >
          {!projectRoot ? (
            <div
              style={{
                flex: 1,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                background: theme.shell.emptyStateBg,
                color: theme.text.primary,
              }}
            >
              <div style={{ maxWidth: 520 }}>
                <h1
                  style={{
                    fontSize: "24px",
                    marginBottom: "0.5rem",
                    fontWeight: 600,
                  }}
                >
                  Welcome to Circuit Scope
                </h1>
                <p
                  style={{
                    fontSize: "14px",
                    color: theme.text.secondary,
                    marginBottom: "1rem",
                  }}
                >
                  Open a Verilog project folder to start browsing and editing
                  files, or open a single file directly.
                </p>
                <div
                  style={{
                    display: "flex",
                    gap: `${theme.space[4]}px`,
                    marginBottom: "1.5rem",
                    flexWrap: "wrap",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      alignItems: "center",
                      gap: `${theme.space[1]}px`,
                    }}
                  >
                    <IconButton
                      variant="large"
                      label="Open folder"
                      onClick={() => void handleOpenFolder()}
                    >
                      <FolderOpen size={28} strokeWidth={1.5} />
                    </IconButton>
                    <span style={{ fontSize: 12, color: theme.text.muted }}>Open folder</span>
                  </div>
                  <div
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      alignItems: "center",
                      gap: `${theme.space[1]}px`,
                    }}
                  >
                    <IconButton
                      variant="large"
                      label="Open file"
                      onClick={() => void handleOpenFile()}
                    >
                      <FileText size={28} strokeWidth={1.5} />
                    </IconButton>
                    <span style={{ fontSize: 12, color: theme.text.muted }}>Open file</span>
                  </div>
                </div>
                {recentFolders.length > 0 && (
                  <div>
                    <div
                      style={{
                        fontSize: "12px",
                        textTransform: "uppercase",
                        color: theme.text.muted,
                        marginBottom: "0.35rem",
                      }}
                    >
                      Recent folders
                    </div>
                    <ul
                      style={{
                        listStyle: "none",
                        margin: 0,
                        padding: 0,
                        fontSize: "13px",
                      }}
                    >
                      {recentFolders.map((folder) => (
                        <li key={folder}>
                          <button
                            type="button"
                            onClick={() => openFolderAt(folder)}
                            style={{
                              background: "none",
                              border: "none",
                              padding: `${theme.space[1] / 4}px 0`,
                              cursor: "pointer",
                              color: theme.text.secondary,
                              textAlign: "left",
                              width: "100%",
                            }}
                          >
                            <span style={{ color: theme.text.primary }}>
                              {folder.replace(/^.*[/\\]/, "") || folder}
                            </span>
                            <span
                              style={{
                                color: theme.text.muted,
                                fontSize: "11px",
                                marginLeft: "0.35rem",
                              }}
                            >
                              {folder}
                            </span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
              </div>
            </div>
          ) : (
            <>
              {/* Scroll happens inside CodeMirror / waveform pane only; terminal stays docked. */}
              <div
                style={{
                  flex: 1,
                  minHeight: 0,
                  minWidth: 0,
                  display: "flex",
                  flexDirection: "column",
                  overflow: "hidden",
                }}
              >
                {projectRoot && waveformVisible && waveformPath ? (
                  <>
                    {debuggerSessionId != null && (
                      <DebuggerToolbar
                        projectRoot={projectRoot}
                        onSessionStarted={(p) => {
                          setWaveformPath(p);
                          setWaveformVisible(true);
                          setWaveformMountKey((k) => k + 1);
                        }}
                        onToast={pushToast}
                      />
                    )}
                    <WaveformPanel
                      key={waveformMountKey}
                      projectRoot={projectRoot}
                      vcdPath={waveformPath}
                      onToast={pushToast}
                      onClose={() => {
                        if (debuggerSessionId != null) {
                          void stopDebugger();
                        } else {
                          setWaveformVisible(false);
                          setWaveformPath(null);
                          void invoke("vcd_close").catch(() => {});
                        }
                      }}
                    />
                  </>
                ) : (
                  <div
                    style={{
                      flex: 1,
                      minHeight: 0,
                      minWidth: 0,
                      display: "flex",
                      flexDirection: "column",
                      background: theme.shell.panelRaised,
                      borderRadius: theme.radius.lg,
                      margin: `${theme.space[2]}px`,
                      border: `1px solid ${theme.shell.panelBorder}`,
                      overflow: "hidden",
                      boxShadow: "inset 0 1px 0 rgba(255,255,255,0.04)",
                    }}
                  >
                    <VerilogEditor
                      fileKey={file?.path ?? "__empty__"}
                      value={file?.content ?? ""}
                      editable={!!file}
                      fontSizePx={editorFontSizePx}
                      filePath={file?.path ?? null}
                      highlightVerilog={
                        !!file && /\.(v|sv)$/i.test(file.path)
                      }
                      onChange={(v) => {
                        if (!file) return;
                        setFile({ ...file, content: v });
                        setIsDirty(true);
                      }}
                    />
                  </div>
                )}
              </div>
              {terminalVisible && (
                <div
                  style={{
                    flexShrink: 0,
                    flexGrow: 0,
                    borderTop: `1px solid ${theme.shell.terminalBorder}`,
                    background: theme.shell.terminalChromeBg,
                    maxHeight: "42vh",
                    height: 268,
                    minHeight: 140,
                    display: "flex",
                    flexDirection: "column",
                  }}
                >
                  <div
                    style={{
                      padding: `${theme.space[1]}px ${theme.space[3]}px`,
                      borderBottom: `1px solid ${theme.shell.terminalBorder}`,
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      fontSize: "12px",
                      color: theme.text.secondary,
                      flexShrink: 0,
                    }}
                  >
                    <span>Terminal</span>
                  </div>
                  <div style={{ flex: 1, minHeight: 0, position: "relative" }}>
                    <TerminalPane
                      key={terminalInstanceId}
                      projectRoot={projectRoot}
                    />
                  </div>
                </div>
              )}
            </>
          )}
        </main>
      </div>
      <footer
        style={{
          flexShrink: 0,
          padding: `${theme.space[1] + 2}px ${theme.space[3]}px`,
          borderTop: `1px solid ${theme.shell.headerBorder}`,
          background: theme.shell.headerBg,
          color: theme.text.muted,
          fontSize: 11,
          letterSpacing: "0.02em",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: `${theme.space[2]}px`,
          userSelect: "none",
        }}
      >
        <span>Circuit Scope — Verilog</span>
        <span style={{ fontVariantNumeric: "tabular-nums" }}>{APP_VERSION}</span>
      </footer>
      {toastStack}
      {contextMenu && (
        <div
          style={{
            position: "fixed",
            inset: 0,
            zIndex: 1000,
          }}
          onClick={() => setContextMenu(null)}
        >
          <div
            style={{
              position: "absolute",
              top: contextMenu.y,
              left: contextMenu.x,
              background: theme.shell.contextMenuBg,
              border: `1px solid ${theme.shell.contextMenuBorder}`,
              borderRadius: theme.radius.sm,
              boxShadow: theme.shell.contextMenuShadow,
              padding: "2px 0",
              fontSize: "12px",
              minWidth: 120,
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <button
              type="button"
              className="cs-menu-item"
              onClick={() => startRename(contextMenu.path)}
            >
              Rename
            </button>
            <button
              type="button"
              className="cs-menu-item cs-menu-item-danger"
              onClick={() => handleDeletePath(contextMenu.path)}
            >
              Delete
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
