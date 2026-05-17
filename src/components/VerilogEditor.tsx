import { useMemo } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { EditorView } from "@codemirror/view";
import { StreamLanguage } from "@codemirror/language";
import { verilog } from "@codemirror/legacy-modes/mode/verilog";
import { theme } from "../ui/theme";
import { debuggerExtension } from "./editor/debuggerExtension";

type Props = {
  value: string;
  onChange: (value: string) => void;
  /** When false, editor is read-only and empty state is shown */
  editable: boolean;
  /** File path for remount when switching documents */
  fileKey: string;
  /** Verilog (IEEE 1364) highlighting for .v / .sv (same RTL subset); plain text otherwise */
  highlightVerilog: boolean;
  /** Editor font size in CSS px (e.g. adjusted with Cmd/Ctrl ±) */
  fontSizePx?: number;
  /** Absolute path of the file in this editor, forwarded to the debugger extension. */
  filePath?: string | null;
};

function createCircuitScopeEditorTheme(fontSizePx: number) {
  const e = theme.editor;
  const shell = theme.shell;
  /** Line spacing scales with font so Cmd ± tightens/loosens both. */
  const lineHeightPx = Math.round(Math.max(16, fontSizePx * 1.58));

  return EditorView.theme(
    {
      /** Theme rules are scoped under the editor root; `&` is that root (the `.cm-editor` element). */
      "&": {
        flex: "1 1 0%",
        minHeight: 0,
        minWidth: 0,
        height: "100%",
        maxHeight: "100%",
        display: "flex",
        flexDirection: "row",
        alignItems: "stretch",
        backgroundColor: shell.editorBg,
        fontSize: `${fontSizePx}px`,
        fontFamily: theme.font.mono,
      },
      ".cm-scroller": {
        flex: "1 1 0%",
        minWidth: 0,
        minHeight: 0,
        overflow: "auto",
        fontFamily: "inherit",
        fontSize: "inherit",
      },
      ".cm-content": {
        caretColor: theme.accent.primary,
        fontSize: `${fontSizePx}px`,
        lineHeight: `${lineHeightPx}px`,
        paddingTop: `${theme.space[3]}px`,
        paddingBottom: `${theme.space[3]}px`,
        paddingLeft: `${theme.space[2]}px`,
        paddingRight: `${theme.space[3]}px`,
      },
      ".cm-line": {
        lineHeight: `${lineHeightPx}px`,
      },
      ".cm-activeLine": {
        lineHeight: `${lineHeightPx}px`,
        backgroundColor: "rgba(255, 255, 255, 0.04)",
      },
      ".cm-gutters": {
        flexShrink: 0,
        backgroundColor: shell.panelRaised,
        color: theme.text.muted,
        border: "none",
        paddingRight: "2px",
        fontSize: `${fontSizePx}px`,
      },
      ".cm-gutterElement": { padding: "0 6px 0 4px" },
      ".cm-activeLineGutter": {
        backgroundColor: "rgba(255, 255, 255, 0.04)",
        lineHeight: `${lineHeightPx}px`,
      },
      ".cm-selectionBackground": {
        backgroundColor: theme.terminal.selection,
      },
      "&.cm-focused .cm-selectionBackground": {
        backgroundColor: theme.terminal.selection,
      },
      ".cm-cursor, &.cm-focused .cm-cursor": {
        borderLeftColor: theme.accent.primary,
      },
      ".cm-keyword": { color: e.keyword },
      ".cm-atom": { color: e.builtin },
      ".cm-number": { color: e.number },
      ".cm-def": { color: e.def },
      ".cm-variable": { color: e.variable },
      ".cm-variable-2": { color: e.meta },
      ".cm-variable-3": { color: e.meta },
      ".cm-property": { color: e.def },
      ".cm-operator": { color: e.operator },
      ".cm-comment": { color: e.comment },
      ".cm-string": { color: e.string },
      ".cm-string-2": { color: e.string },
      ".cm-meta": { color: e.meta },
      ".cm-qualifier": { color: e.keyword },
      ".cm-builtin": { color: e.builtin },
      ".cm-tag": { color: e.tag },
      ".cm-attribute": { color: e.def },
      ".cm-hr": { color: e.hr },
      ".cm-link": { color: e.link, textDecoration: "underline" },
      ".cm-error": { color: e.error },
      ".cm-matchingBracket": {
        color: theme.accent.primary,
        fontWeight: "bold",
      },
    },
    { dark: true },
  );
}

export function VerilogEditor({
  value,
  onChange,
  editable,
  fileKey,
  highlightVerilog,
  fontSizePx = 14,
  filePath = null,
}: Props) {
  const extensions = useMemo(
    () => [
      ...(highlightVerilog ? [StreamLanguage.define(verilog)] : []),
      createCircuitScopeEditorTheme(fontSizePx),
      debuggerExtension({ filePath }),
    ],
    [highlightVerilog, fontSizePx, filePath],
  );

  return (
    <div
      className="verilog-editor-host"
      style={{
        flex: "1 1 0%",
        minHeight: 0,
        minWidth: 0,
        display: "flex",
        flexDirection: "column",
        overflow: "hidden",
      }}
    >
      <CodeMirror
        key={fileKey}
        value={value}
        height="100%"
        style={{ flex: "1 1 0%", minHeight: 0, minWidth: 0, overflow: "hidden" }}
        theme="none"
        extensions={extensions}
        basicSetup={{
          tabSize: 2,
          highlightActiveLine: true,
          highlightActiveLineGutter: true,
          foldGutter: true,
          dropCursor: true,
          allowMultipleSelections: true,
          indentOnInput: true,
          bracketMatching: true,
          closeBrackets: true,
          autocompletion: true,
          rectangularSelection: true,
          crosshairCursor: true,
          highlightSelectionMatches: true,
          searchKeymap: true,
        }}
        indentWithTab
        editable={editable}
        onChange={onChange}
      />
    </div>
  );
}
