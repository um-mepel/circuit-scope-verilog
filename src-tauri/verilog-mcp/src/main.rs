//! Model Context Protocol server for the Circuit Scope Verilog compiler.
//!
//! Exposes `verilog-core` parsing, simulation, VCD inspection, and the
//! stepping debugger over MCP stdio so other Claude Code sessions (or any
//! MCP client) can drive the compiler programmatically.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::schemars::{self, JsonSchema};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

mod state;

use state::{SessionEntry, SimSessions};

use wellen::viewers::{read_body, read_header_from_file};
use wellen::{
    Hierarchy, LoadOptions, ScopeOrVarRef, ScopeRef, SignalRef, TimescaleUnit,
};

// ============================================================================
// Server
// ============================================================================

#[derive(Clone)]
pub struct VerilogMcpServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    sim_sessions: Arc<Mutex<SimSessions>>,
}

// ============================================================================
// Argument types
// ============================================================================

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathArgs {
    /// Absolute path to a Verilog source file (`.v` or `.sv`).
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RootArgs {
    /// Absolute path to a project directory containing Verilog sources.
    pub root: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimulateVcdArgs {
    /// Absolute path to a project directory containing Verilog sources.
    pub root: String,
    /// Number of simulation cycles; auto-derived from initial delays if omitted.
    pub cycles: Option<usize>,
    /// Output VCD file name (no path separators). Defaults to `circuit_scope.vcd`.
    pub output_file: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VcdQueryArgs {
    /// Absolute path to the `.vcd` file.
    pub path: String,
    /// Wellen signal-ref indices, e.g. the `signalId` values returned by `vcd_info`.
    pub signal_ids: Vec<u32>,
    pub t_start: u64,
    pub t_end: u64,
    /// Cap on transitions returned per signal (default 65536).
    pub max_points_per_signal: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VcdFindEdgeArgs {
    pub path: String,
    pub signal_id: u32,
    pub from_time: u64,
    /// `true` searches forward (next edge after `from_time`); `false` searches backward.
    pub next: bool,
    /// `"any" | "rising" | "falling"`.
    pub edge_kind: String,
    /// LSB-indexed bit position within a bus signal; omit for whole-signal edges.
    pub bit_lsb: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimStartArgs {
    /// Absolute path to a project directory.
    pub project_root: String,
    /// Top module name; auto-detected if omitted.
    pub top_module: Option<String>,
    /// Number of simulation cycles for the run length (default 64).
    pub num_cycles: Option<usize>,
    /// VCD file name (no path separators) written under `project_root`.
    pub vcd_filename: Option<String>,
}

// BreakpointDto removed for 0.3.0 — restore from git history when the
// breakpoint feature is re-enabled in verilog-core.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimStepArgs {
    pub session_id: u64,
    /// `"statement" | "tick" | "cycle" | "run"`.
    pub mode: String,
    /// Hierarchical clock signal name for `cycle` mode, e.g. `top.clk`.
    pub clock_signal: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimQueryActiveArgs {
    pub session_id: u64,
    pub time_fs: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimEvalArgs {
    pub session_id: u64,
    /// Identifier as written in source; may be bare (`x`) or hierarchical (`top.u1.x`).
    pub identifier: String,
    /// Source file containing the identifier; helps disambiguate non-top scopes.
    pub file_path: Option<String>,
    /// Evaluate at this simulator time (fs). Omit for the current time.
    pub time_fs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimDriverAtArgs {
    pub session_id: u64,
    /// Fully-qualified signal name as it appears in the simulator (e.g. `top.u1.q`).
    pub signal: String,
    pub time_fs: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimEndArgs {
    pub session_id: u64,
    /// When `false`, delete the VCD file at session end. Default `true`.
    pub keep_vcd: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionIdArgs {
    pub session_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimEvalManyArgs {
    pub session_id: u64,
    /// Identifiers as written in source; each is resolved with the same
    /// fallback chain as `sim_eval` (as-typed, then top-prefixed, then
    /// file-module prefixed when `file_path` is provided).
    pub identifiers: Vec<String>,
    pub file_path: Option<String>,
    pub time_fs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceExcerptArgs {
    /// Absolute path to a source file.
    pub path: String,
    /// Inclusive byte offset of the first character.
    pub start: u32,
    /// Exclusive byte offset; characters in `[start, end)` are returned.
    pub end: u32,
    /// Optional number of surrounding lines to include for context on each
    /// side of the span (default 0 — return only the span itself).
    pub context_lines: Option<u32>,
}

// ============================================================================
// Helpers
// ============================================================================

fn ok_json<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let s = serde_json::to_string(value)
        .map_err(|e| err_internal(format!("serialize response: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn err_internal(msg: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(msg.into(), None)
}

fn err_invalid(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

fn parse_step_mode(raw: &str) -> Result<verilog_core::StepMode, String> {
    match raw {
        "statement" | "OneStatement" => Ok(verilog_core::StepMode::OneStatement),
        "tick" | "OneTick" => Ok(verilog_core::StepMode::OneTick),
        "cycle" | "OneCycle" => Ok(verilog_core::StepMode::OneCycle),
        "run" | "ToEnd" => Ok(verilog_core::StepMode::ToEnd),
        other => Err(format!("unknown step mode: {other}")),
    }
}

fn span_info_to_json(info: verilog_core::SpanInfo) -> serde_json::Value {
    serde_json::json!({
        "fileId": info.file_id,
        "path": info.path,
        "start": info.start,
        "end": info.end,
        "lineStart": info.line_start,
        "colStart": info.col_start,
        "lineEnd": info.line_end,
        "colEnd": info.col_end,
    })
}

// Breakpoint helpers (resolve_breakpoints, hit_to_json) removed for 0.3.0.

// ---------- wellen / VCD helpers (ported from src-tauri/src/vcd_viewer.rs) ----------

fn timescale_unit_label(u: TimescaleUnit) -> &'static str {
    match u {
        TimescaleUnit::ZeptoSeconds => "zs",
        TimescaleUnit::AttoSeconds => "as",
        TimescaleUnit::FemtoSeconds => "fs",
        TimescaleUnit::PicoSeconds => "ps",
        TimescaleUnit::NanoSeconds => "ns",
        TimescaleUnit::MicroSeconds => "us",
        TimescaleUnit::MilliSeconds => "ms",
        TimescaleUnit::Seconds => "s",
        TimescaleUnit::Unknown => "unknown",
    }
}

fn scope_subtree(h: &Hierarchy, sref: ScopeRef) -> serde_json::Value {
    let s = &h[sref];
    let name = s.name(h).to_string();
    let mut scopes = Vec::new();
    let mut vars = Vec::new();
    for item in s.items(h) {
        match item {
            ScopeOrVarRef::Scope(child) => scopes.push(scope_subtree(h, child)),
            ScopeOrVarRef::Var(vr) => {
                let v = &h[vr];
                let bits = v.length().unwrap_or(1);
                vars.push(serde_json::json!({
                    "signalId": v.signal_ref().index() as u32,
                    "name": v.name(h).to_string(),
                    "bits": bits,
                }));
            }
        }
    }
    serde_json::json!({
        "name": name,
        "scopes": scopes,
        "vars": vars,
    })
}

fn hierarchy_tree(h: &Hierarchy) -> Vec<serde_json::Value> {
    h.scopes().map(|sref| scope_subtree(h, sref)).collect()
}

fn bits_for_signal(h: &Hierarchy, want: SignalRef) -> u32 {
    fn walk(h: &Hierarchy, sref: ScopeRef, want: SignalRef) -> Option<u32> {
        let s = &h[sref];
        for item in s.items(h) {
            match item {
                ScopeOrVarRef::Scope(ch) => {
                    if let Some(b) = walk(h, ch, want) {
                        return Some(b);
                    }
                }
                ScopeOrVarRef::Var(vr) => {
                    let v = &h[vr];
                    if v.signal_ref() == want {
                        return Some(v.length().unwrap_or(1));
                    }
                }
            }
        }
        None
    }
    for top in h.scopes() {
        if let Some(b) = walk(h, top, want) {
            return b;
        }
    }
    1
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Any,
    Rising,
    Falling,
}

fn parse_bin_u128(s: &str) -> Option<u128> {
    let t = s.trim();
    if t.is_empty() || !t.chars().all(|c| c == '0' || c == '1') {
        return None;
    }
    u128::from_str_radix(t, 2).ok()
}

fn bit01(s: &str) -> Option<u8> {
    let t = s.trim();
    match t {
        "0" => Some(0),
        "1" => Some(1),
        _ => None,
    }
}

fn edge_matches(prev: &str, new: &str, bits: u32, kind: EdgeKind) -> bool {
    if prev == new {
        return false;
    }
    match kind {
        EdgeKind::Any => true,
        EdgeKind::Rising => {
            if bits <= 1 {
                bit01(prev) == Some(0) && bit01(new) == Some(1)
            } else {
                match (parse_bin_u128(prev), parse_bin_u128(new)) {
                    (Some(a), Some(b)) => b > a,
                    _ => false,
                }
            }
        }
        EdgeKind::Falling => {
            if bits <= 1 {
                bit01(prev) == Some(1) && bit01(new) == Some(0)
            } else {
                match (parse_bin_u128(prev), parse_bin_u128(new)) {
                    (Some(a), Some(b)) => b < a,
                    _ => false,
                }
            }
        }
    }
}

fn normalize_bus_binary_msb(s: &str, width: u32) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || !t.chars().all(|c| c == '0' || c == '1') {
        return None;
    }
    let w = width as usize;
    if t.len() >= w {
        Some(t[t.len() - w..].to_string())
    } else {
        Some(format!("{:0>width$}", t, width = w))
    }
}

fn bit_at_lsb_index(bin_msb: &str, bit_lsb: u32, width: u32) -> Option<u8> {
    let i = (width as usize).checked_sub(1)?.checked_sub(bit_lsb as usize)?;
    match bin_msb.as_bytes().get(i)? {
        b'0' => Some(0),
        b'1' => Some(1),
        _ => None,
    }
}

fn edge_matches_lsb_bit(
    prev: &str,
    new: &str,
    width: u32,
    bit_lsb: u32,
    kind: EdgeKind,
) -> bool {
    let Some(pb) =
        normalize_bus_binary_msb(prev, width).and_then(|b| bit_at_lsb_index(&b, bit_lsb, width))
    else {
        return false;
    };
    let Some(nb) =
        normalize_bus_binary_msb(new, width).and_then(|b| bit_at_lsb_index(&b, bit_lsb, width))
    else {
        return false;
    };
    if pb == nb {
        return false;
    }
    match kind {
        EdgeKind::Any => true,
        EdgeKind::Rising => pb == 0 && nb == 1,
        EdgeKind::Falling => pb == 1 && nb == 0,
    }
}

// ============================================================================
// Tool implementations
// ============================================================================

#[tool_router]
impl VerilogMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            sim_sessions: Arc::new(Mutex::new(SimSessions::default())),
        }
    }

    #[tool(description = "Return verilog-core compiler version metadata.")]
    async fn compiler_info(
        &self,
        Parameters(_): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let payload = serde_json::json!({
            "verilogCoreVersion": verilog_core::PACKAGE_VERSION,
            "mcpServerVersion": env!("CARGO_PKG_VERSION"),
        });
        ok_json(&payload)
    }

    #[tool(description = "Parse a single Verilog source file. Returns modules and diagnostics.")]
    async fn parse_file(
        &self,
        Parameters(args): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| err_internal(format!("read {}: {e}", args.path)))?;
        let result = verilog_core::parse_file(args.path.clone(), &content);
        ok_json(&result)
    }

    #[tool(description = "Walk `root` for `.v`/`.sv` files and return every module name + source path discovered.")]
    async fn index_project(
        &self,
        Parameters(args): Parameters<RootArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let idx = verilog_core::index_project(Path::new(&args.root))
            .map_err(|e| err_internal(format!("index project: {e}")))?;
        ok_json(&idx)
    }

    #[tool(description = "Semantic analysis over `root`: per-module ports, nets, instances, assigns, plus the list of candidate top modules (modules nobody instantiates).")]
    async fn analyze_project(
        &self,
        Parameters(args): Parameters<RootArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let project = verilog_core::analyze_project(Path::new(&args.root))
            .map_err(|e| err_internal(format!("analyze project: {e}")))?;
        let modules_json: Vec<serde_json::Value> = project
            .modules
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "path": m.path,
                    "ports": m.ports,
                    "nets": m.nets,
                    "instances": m.instances.iter().map(|i| serde_json::json!({
                        "moduleName": i.module_name,
                        "instanceName": i.instance_name,
                    })).collect::<Vec<_>>(),
                    "assigns": m.assigns.iter().map(|a| serde_json::json!({
                        "lhs": a.lhs,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        ok_json(&serde_json::json!({
            "modules": modules_json,
            "diagnostics": project.diagnostics,
            "topModules": project.top_modules,
        }))
    }

    #[tool(description = "Elaborate the project and report the auto-detected top module (the same heuristic `sim_start` and `simulate_vcd` use when `top_module` is unspecified).")]
    async fn find_top_module(
        &self,
        Parameters(args): Parameters<RootArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let project = verilog_core::build_ir_for_root(Path::new(&args.root))
            .map_err(|e| err_internal(format!("build IR: {e}")))?;
        let name = verilog_core::find_top_module(&project).map_err(err_internal)?;
        let path = project
            .modules
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.path.clone());
        ok_json(&serde_json::json!({
            "name": name,
            "path": path,
        }))
    }

    #[tool(description = "Compile + simulate the project at `root` and write a VCD file. Returns the absolute path of the written VCD. Matches Circuit Scope's `File > Generate VCD` menu.")]
    async fn simulate_vcd(
        &self,
        Parameters(args): Parameters<SimulateVcdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root_path = Path::new(&args.root);
        let name = args
            .output_file
            .unwrap_or_else(|| "circuit_scope.vcd".to_string());
        if name.contains('/') || name.contains('\\') || name.is_empty() {
            return Err(err_invalid(
                "output_file must be a file name only, no path separators",
            ));
        }
        let paths = verilog_core::list_verilog_source_paths(root_path)
            .map_err(|e| err_internal(format!("list sources: {e}")))?;
        if paths.is_empty() {
            return Err(err_invalid(
                "No Verilog sources (.v or .sv) found under this folder.",
            ));
        }
        let out_path = root_path.join(&name);
        let vcd = verilog_core::run_csverilog_pipeline(
            &paths,
            &out_path,
            "verilog-mcp simulate_vcd",
            verilog_core::CsVerilogOptions {
                num_cycles: args.cycles,
                ..Default::default()
            },
        )
        .map_err(err_internal)?;
        std::fs::write(&out_path, &vcd)
            .map_err(|e| err_internal(format!("write VCD: {e}")))?;
        ok_json(&serde_json::json!({
            "vcdPath": out_path.to_string_lossy(),
            "bytes": vcd.len(),
        }))
    }

    #[tool(description = "Open a VCD file and return its timescale, time range, and full scope/signal hierarchy. Each leaf signal has a numeric `signalId` you pass to `vcd_query` / `vcd_find_edge`.")]
    async fn vcd_info(
        &self,
        Parameters(args): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let fp = Path::new(&args.path);
        let opts = LoadOptions {
            multi_thread: false,
            remove_scopes_with_empty_name: false,
        };
        let header =
            read_header_from_file(fp, &opts).map_err(|e| err_internal(format!("vcd header: {e}")))?;
        let hierarchy = header.hierarchy;
        let body = read_body(header.body, &hierarchy, None)
            .map_err(|e| err_internal(format!("vcd body: {e}")))?;
        let time_start = body.time_table.first().copied().unwrap_or(0);
        let time_end = body.time_table.last().copied().unwrap_or(0);
        let (ts_factor, ts_unit) = match hierarchy.timescale() {
            Some(t) => (
                Some(t.factor),
                Some(timescale_unit_label(t.unit).to_string()),
            ),
            None => (None, None),
        };
        let tree = hierarchy_tree(&hierarchy);
        ok_json(&serde_json::json!({
            "path": args.path,
            "timeStart": time_start,
            "timeEnd": time_end,
            "timescaleFactor": ts_factor,
            "timescaleUnit": ts_unit,
            "hierarchy": tree,
        }))
    }

    #[tool(description = "Return signal transitions inside `[t_start, t_end]` for the requested signals. Each entry has `{signalId, time, value}` with `value` as a string (binary for buses, `0`/`1` for scalars, or VCD x/z encodings).")]
    async fn vcd_query(
        &self,
        Parameters(args): Parameters<VcdQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let fp = Path::new(&args.path);
        let opts = LoadOptions {
            multi_thread: false,
            remove_scopes_with_empty_name: false,
        };
        let header = read_header_from_file(fp, &opts)
            .map_err(|e| err_internal(format!("vcd header: {e}")))?;
        let hierarchy = header.hierarchy;
        let body = read_body(header.body, &hierarchy, None)
            .map_err(|e| err_internal(format!("vcd body: {e}")))?;
        let tt = &body.time_table;
        let mut source = body.source;

        let max_pts = args.max_points_per_signal.unwrap_or(65_536).max(32);
        let tt_min = tt.first().copied().unwrap_or(0);
        let tt_max = tt.last().copied().unwrap_or(tt_min);
        let (raw0, raw1) = (
            args.t_start.min(args.t_end),
            args.t_start.max(args.t_end),
        );
        let t_start = raw0.clamp(tt_min, tt_max);
        let t_end = raw1.clamp(tt_min, tt_max);

        let refs: Vec<SignalRef> = args
            .signal_ids
            .into_iter()
            .filter_map(|i| SignalRef::from_index(i as usize))
            .collect();
        if refs.is_empty() {
            return ok_json(&serde_json::json!({"transitions": []}));
        }

        let signals = source.load_signals(&refs, &hierarchy, false);
        let mut transitions: Vec<serde_json::Value> = Vec::new();

        for (sig_ref, signal) in signals {
            let sid = sig_ref.index() as u32;
            let mut local: Vec<(u64, String)> = Vec::new();
            let mut last_before_start: Option<String> = None;
            for (time_idx, value) in signal.iter_changes() {
                let ti = time_idx as usize;
                if ti >= tt.len() {
                    continue;
                }
                let t = tt[ti];
                let value_str = format!("{value}");
                if t < t_start {
                    last_before_start = Some(value_str);
                    continue;
                }
                if t > t_end {
                    break;
                }
                local.push((t, value_str));
            }
            if local.is_empty() {
                if let Some(v) = last_before_start {
                    local.push((t_start, v));
                }
            } else if local[0].0 > t_start {
                if let Some(v) = last_before_start {
                    local.insert(0, (t_start, v));
                }
            }
            if local.len() > max_pts {
                local = subsample_uniform(&local, max_pts, t_start, t_end);
            }
            for (time, value) in local {
                transitions.push(serde_json::json!({
                    "signalId": sid,
                    "time": time,
                    "value": value,
                }));
            }
        }

        ok_json(&serde_json::json!({"transitions": transitions}))
    }

    #[tool(description = "Find the next or previous edge for one signal. `edgeKind` is `any | rising | falling`. Returns the matching time, or null if none.")]
    async fn vcd_find_edge(
        &self,
        Parameters(args): Parameters<VcdFindEdgeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let fp = Path::new(&args.path);
        let opts = LoadOptions {
            multi_thread: false,
            remove_scopes_with_empty_name: false,
        };
        let header = read_header_from_file(fp, &opts)
            .map_err(|e| err_internal(format!("vcd header: {e}")))?;
        let hierarchy = header.hierarchy;
        let body = read_body(header.body, &hierarchy, None)
            .map_err(|e| err_internal(format!("vcd body: {e}")))?;
        let tt = &body.time_table;
        let mut source = body.source;

        let sig_ref = SignalRef::from_index(args.signal_id as usize)
            .ok_or_else(|| err_invalid(format!("Invalid signalId {}", args.signal_id)))?;
        let bits = bits_for_signal(&hierarchy, sig_ref);
        let kind = match args.edge_kind.to_lowercase().as_str() {
            "rising" => EdgeKind::Rising,
            "falling" => EdgeKind::Falling,
            _ => EdgeKind::Any,
        };

        let signals = source.load_signals(&[sig_ref], &hierarchy, false);
        let (_, signal) = signals
            .into_iter()
            .next()
            .ok_or_else(|| err_internal("signal not found in VCD"))?;

        let mut points: Vec<(u64, String)> = Vec::new();
        for (time_idx, value) in signal.iter_changes() {
            let ti = time_idx as usize;
            if ti >= tt.len() {
                continue;
            }
            points.push((tt[ti], format!("{value}")));
        }

        if points.len() < 2 {
            return ok_json(&serde_json::json!({"time": serde_json::Value::Null}));
        }

        let matched: Option<u64> = if args.next {
            let mut found = None;
            for i in 1..points.len() {
                let (t_edge, ref new_v) = points[i];
                let (_, ref prev_v) = points[i - 1];
                let ok = match args.bit_lsb {
                    Some(b) if bits > 1 => edge_matches_lsb_bit(prev_v, new_v, bits, b, kind),
                    _ => edge_matches(prev_v, new_v, bits, kind),
                };
                if t_edge > args.from_time && ok {
                    found = Some(t_edge);
                    break;
                }
            }
            found
        } else {
            let mut best = None;
            for i in 1..points.len() {
                let (t_edge, ref new_v) = points[i];
                let (_, ref prev_v) = points[i - 1];
                let ok = match args.bit_lsb {
                    Some(b) if bits > 1 => edge_matches_lsb_bit(prev_v, new_v, bits, b, kind),
                    _ => edge_matches(prev_v, new_v, bits, kind),
                };
                if t_edge < args.from_time && ok {
                    best = Some(t_edge);
                }
            }
            best
        };

        ok_json(&serde_json::json!({"time": matched}))
    }

    #[tool(description = "Start a stepping debugger session: elaborate `project_root`, optimise, start the simulator, write the VCD header to disk. Returns a `sessionId` you pass to subsequent `sim_*` tools.")]
    async fn sim_start(
        &self,
        Parameters(args): Parameters<SimStartArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Acquire the registry lock up front so concurrent `sim_*` tool
        // dispatches don't race ahead with an unknown session id. The lock
        // is released when this future returns; subsequent tool calls then
        // see the freshly inserted session. IR build / optimisation is pure
        // synchronous CPU work and does not `.await`, so holding the lock
        // across it is safe (no deadlock risk).
        let mut state = self.sim_sessions.lock().await;

        let root = PathBuf::from(&args.project_root);
        let mut project = verilog_core::build_ir_for_root(&root)
            .map_err(|e| err_internal(format!("build IR: {e}")))?;
        verilog_core::optimize_project(&mut project);

        let top = match args.top_module {
            Some(t) => t,
            None => verilog_core::find_top_module(&project).map_err(err_internal)?,
        };
        let cycles = args.num_cycles.unwrap_or(64);
        let config = verilog_core::SimConfig {
            top_module: top,
            num_cycles: cycles,
            ..Default::default()
        };

        let vcd_filename = args
            .vcd_filename
            .unwrap_or_else(|| "debug.vcd".to_string());
        if vcd_filename.contains('/') || vcd_filename.contains('\\') {
            return Err(err_invalid("vcdFilename must be a file name only"));
        }
        let vcd_path = root.join(&vcd_filename);

        let mut session = verilog_core::SimSession::start(&project, config, vcd_path.clone())
            .map_err(err_internal)?;
        session
            .flush_vcd()
            .map_err(|e| err_internal(format!("flush vcd: {e}")))?;

        let source_files: Vec<serde_json::Value> = project
            .source_map
            .files()
            .map(|(id, p)| serde_json::json!({"id": id, "path": p}))
            .collect();
        let time_fs = session.current_time_fs();

        let id = state.registry.alloc_id();
        state
            .sessions
            .insert(id, SessionEntry { session, project });

        ok_json(&serde_json::json!({
            "sessionId": id,
            "vcdPath": vcd_path.to_string_lossy(),
            "sourceFiles": source_files,
            "timeFs": time_fs,
        }))
    }

    #[tool(description = "Advance an existing debugger session. `mode` is `statement | tick | cycle | run`. Returns the active source spans at the new time. (Breakpoints are temporarily disabled for 0.3.0; re-enable via git history.)")]
    async fn sim_step(
        &self,
        Parameters(args): Parameters<SimStepArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mode = parse_step_mode(&args.mode).map_err(err_invalid)?;
        let mut state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get_mut(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;

        let outcome = entry.session.step(mode, args.clock_signal.as_deref());
        entry
            .session
            .flush_vcd()
            .map_err(|e| err_internal(format!("flush vcd: {e}")))?;

        let t_fs = entry.session.current_time_fs();
        let active: Vec<_> = entry
            .session
            .active_span_infos_at(&entry.project, t_fs)
            .into_iter()
            .map(span_info_to_json)
            .collect();
        let (ran, done) = match outcome {
            verilog_core::StepOutcome::Advanced { ran_statements, .. } => (ran_statements, false),
            verilog_core::StepOutcome::End => (0, true),
        };

        ok_json(&serde_json::json!({
            "timeFs": t_fs,
            "activeSpans": active,
            "ranStatements": ran,
            "done": done,
        }))
    }

    #[tool(description = "Return source spans active at `time_fs` for an existing session, without advancing the simulator. Useful after scrubbing the waveform cursor.")]
    async fn sim_query_active(
        &self,
        Parameters(args): Parameters<SimQueryActiveArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;
        let active: Vec<_> = entry
            .session
            .active_span_infos_at(&entry.project, args.time_fs)
            .into_iter()
            .map(span_info_to_json)
            .collect();
        ok_json(&serde_json::json!({"activeSpans": active}))
    }

    #[tool(description = "Look up the live value of `identifier` in a running session. Tries the name as-typed, then prefixed with the top module, then prefixed with every module declared in `file_path` if provided.")]
    async fn sim_eval(
        &self,
        Parameters(args): Parameters<SimEvalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;

        let mut candidates: Vec<String> = Vec::new();
        candidates.push(args.identifier.clone());
        let top_name = &entry.session.config().top_module;
        candidates.push(format!("{top_name}.{}", args.identifier));
        if let Some(path) = &args.file_path {
            for m in &entry.project.modules {
                if m.path == *path {
                    candidates.push(format!("{}.{}", m.name, args.identifier));
                }
            }
        }

        for name in &candidates {
            let resolved_eval = match args.time_fs {
                Some(t) => entry.session.eval_signal_at_or_before(name, t),
                None => entry.session.eval_signal_resolved(name),
            };
            if let Some((val, width, resolved)) = resolved_eval {
                let mask: i64 = if width == 0 || width >= 63 {
                    !0
                } else {
                    (1i64 << width) - 1
                };
                let unsigned = (val as u64) & (mask as u64);
                let render = |v: i64| -> (String, String, String) {
                    let u = (v as u64) & (mask as u64);
                    (
                        u.to_string(),
                        format!("0x{:X}", u),
                        format!("0b{:0width$b}", u, width = width.max(1)),
                    )
                };
                let transition_raw = match args.time_fs {
                    Some(t) => entry.session.transition_info_at(&resolved, t),
                    None => None,
                };
                let transition = transition_raw.filter(|(from, to)| from != to);
                let (prev_decimal, prev_hex, prev_binary) = match &transition {
                    Some((from, _)) => {
                        let (d, h, b) = render(*from);
                        (Some(d), Some(h), Some(b))
                    }
                    None => (None, None, None),
                };
                return ok_json(&serde_json::json!({
                    "decimal": unsigned.to_string(),
                    "hex": format!("0x{:X}", unsigned),
                    "binary": format!("0b{:0width$b}", unsigned, width = width.max(1)),
                    "width": width,
                    "resolvedName": resolved,
                    "transitioning": transition.is_some(),
                    "prevDecimal": prev_decimal,
                    "prevHex": prev_hex,
                    "prevBinary": prev_binary,
                }));
            }
        }

        Err(err_invalid(format!(
            "identifier '{}' not found; tried: {:?}",
            args.identifier, candidates
        )))
    }

    #[tool(description = "Resolve the driving statement for a fully-qualified signal at `time_fs`. Returns the statement byte span in its source file.")]
    async fn sim_driver_at(
        &self,
        Parameters(args): Parameters<SimDriverAtArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;
        let res = entry
            .session
            .driver_at(&entry.project, &args.signal, args.time_fs)
            .ok_or_else(|| {
                err_invalid(format!(
                    "could not resolve signal '{}' in the simulator",
                    args.signal
                ))
            })?;
        ok_json(&serde_json::json!({
            "resolvedSignal": res.resolved_signal,
            "fileId": res.file_id,
            "path": res.path,
            "stmtStart": res.stmt_span.start,
            "stmtEnd": res.stmt_span.end,
        }))
    }

    #[tool(description = "List every active debugger session: id, top_module, current simulator time, vcd path. Useful for recovering session ids after a context loss.")]
    async fn sim_list_sessions(
        &self,
        Parameters(_): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let sessions: Vec<serde_json::Value> = state
            .sessions
            .iter()
            .map(|(id, entry)| {
                serde_json::json!({
                    "sessionId": id,
                    "topModule": entry.session.config().top_module,
                    "numCycles": entry.session.config().num_cycles,
                    "timeFs": entry.session.current_time_fs(),
                    "vcdPath": entry.session.vcd_path().to_string_lossy(),
                })
            })
            .collect();
        ok_json(&serde_json::json!({"sessions": sessions}))
    }

    #[tool(description = "Return metadata for one session: top_module, num_cycles, current simulator time, vcd path, and the source files registered in the elaborated project.")]
    async fn sim_state(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;
        let source_files: Vec<serde_json::Value> = entry
            .project
            .source_map
            .files()
            .map(|(id, p)| serde_json::json!({"id": id, "path": p}))
            .collect();
        ok_json(&serde_json::json!({
            "sessionId": args.session_id,
            "topModule": entry.session.config().top_module,
            "numCycles": entry.session.config().num_cycles,
            "timeFs": entry.session.current_time_fs(),
            "vcdPath": entry.session.vcd_path().to_string_lossy(),
            "sourceFiles": source_files,
        }))
    }

    #[tool(description = "Evaluate multiple identifiers against a session in one call. Returns a map of identifier -> evaluation result (or null on resolution failure). Reduces round-trips for watch lists.")]
    async fn sim_eval_many(
        &self,
        Parameters(args): Parameters<SimEvalManyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.sim_sessions.lock().await;
        let entry = state
            .sessions
            .get(&args.session_id)
            .ok_or_else(|| err_invalid(format!("unknown sessionId: {}", args.session_id)))?;
        let top_name = &entry.session.config().top_module;
        let mut results = serde_json::Map::new();
        for identifier in &args.identifiers {
            let mut candidates: Vec<String> = Vec::with_capacity(4);
            candidates.push(identifier.clone());
            candidates.push(format!("{top_name}.{}", identifier));
            if let Some(path) = &args.file_path {
                for m in &entry.project.modules {
                    if m.path == *path {
                        candidates.push(format!("{}.{}", m.name, identifier));
                    }
                }
            }
            let mut value: Option<serde_json::Value> = None;
            for name in &candidates {
                let resolved = match args.time_fs {
                    Some(t) => entry.session.eval_signal_at_or_before(name, t),
                    None => entry.session.eval_signal_resolved(name),
                };
                if let Some((val, width, resolved_name)) = resolved {
                    let mask: i64 = if width == 0 || width >= 63 {
                        !0
                    } else {
                        (1i64 << width) - 1
                    };
                    let unsigned = (val as u64) & (mask as u64);
                    value = Some(serde_json::json!({
                        "decimal": unsigned.to_string(),
                        "hex": format!("0x{:X}", unsigned),
                        "binary": format!("0b{:0width$b}", unsigned, width = width.max(1)),
                        "width": width,
                        "resolvedName": resolved_name,
                    }));
                    break;
                }
            }
            results.insert(
                identifier.clone(),
                value.unwrap_or(serde_json::Value::Null),
            );
        }
        ok_json(&serde_json::Value::Object(results))
    }

    #[tool(description = "Read a slice of a source file by byte offset. Pair with `sim_query_active` / `sim_driver_at` results to fetch the exact source text the simulator is reporting without `Read`-ing the whole file.")]
    async fn sim_source_excerpt(
        &self,
        Parameters(args): Parameters<SourceExcerptArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| err_internal(format!("read {}: {e}", args.path)))?;
        let start = args.start as usize;
        let end = (args.end as usize).min(content.len());
        if start > end || start > content.len() {
            return Err(err_invalid(format!(
                "byte range {}..{} out of bounds for file of length {}",
                args.start,
                args.end,
                content.len()
            )));
        }
        // Bump to UTF-8 char boundaries so the slice is safe even when the
        // file has multi-byte characters in/near the span.
        let mut start_b = start;
        while !content.is_char_boundary(start_b) && start_b > 0 {
            start_b -= 1;
        }
        let mut end_b = end;
        while !content.is_char_boundary(end_b) && end_b < content.len() {
            end_b += 1;
        }
        let excerpt = &content[start_b..end_b];

        // Compute 1-indexed line/column at the start of the span (handy for
        // editor jump-to-line in agent outputs).
        let prefix = &content[..start_b];
        let line_start = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let col_start = prefix
            .rsplit('\n')
            .next()
            .map(|s| s.chars().count())
            .unwrap_or(0)
            + 1;

        // Optional context lines on either side.
        let ctx = args.context_lines.unwrap_or(0) as usize;
        let context_text = if ctx > 0 {
            let lines: Vec<&str> = content.lines().collect();
            let lo = line_start.saturating_sub(ctx + 1).min(lines.len());
            let hi = (line_start + ctx).min(lines.len());
            Some(
                lines[lo..hi]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{:>4} | {}", lo + i + 1, l))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            None
        };

        let mut payload = serde_json::json!({
            "path": args.path,
            "start": args.start,
            "end": args.end,
            "lineStart": line_start,
            "colStart": col_start,
            "excerpt": excerpt,
        });
        if let Some(c) = context_text {
            payload["context"] = serde_json::Value::String(c);
        }
        ok_json(&payload)
    }

        #[tool(description = "End a debugger session. By default the VCD file is preserved so it can still be opened with `vcd_info`/`vcd_query`. Pass `keep_vcd: false` to delete it.")]
    async fn sim_end(
        &self,
        Parameters(args): Parameters<SimEndArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let keep = args.keep_vcd.unwrap_or(true);
        let mut state = self.sim_sessions.lock().await;
        if let Some(entry) = state.sessions.remove(&args.session_id) {
            let path = entry.session.vcd_path().to_path_buf();
            if !keep {
                let _ = std::fs::remove_file(&path);
            }
            ok_json(&serde_json::json!({
                "ok": true,
                "vcdPath": path.to_string_lossy(),
                "kept": keep,
            }))
        } else {
            Err(err_invalid(format!(
                "unknown sessionId: {}",
                args.session_id
            )))
        }
    }
}

// ============================================================================
// Decimation helper for vcd_query
// ============================================================================

fn subsample_uniform(
    rows: &[(u64, String)],
    max: usize,
    t_start: u64,
    t_end: u64,
) -> Vec<(u64, String)> {
    if rows.is_empty() {
        return vec![];
    }
    if rows.len() <= max || max < 2 {
        return rows.to_vec();
    }
    let span = t_end.saturating_sub(t_start).max(1) as u128;
    let denom = (max - 1).max(1) as u128;

    let value_at_or_before = |t: u64| -> Option<&str> {
        if rows.is_empty() {
            return None;
        }
        let idx = rows.partition_point(|r| r.0 <= t);
        if idx == 0 {
            None
        } else {
            Some(rows[idx - 1].1.as_str())
        }
    };

    let mut samples: Vec<(u64, String)> = Vec::with_capacity(max);
    for k in 0..max {
        let t = if max <= 1 {
            t_start
        } else {
            let off = (span * (k as u128)) / denom;
            t_start.saturating_add(off as u64)
        };
        let v = value_at_or_before(t)
            .unwrap_or_else(|| rows[0].1.as_str())
            .to_string();
        samples.push((t, v));
    }
    // Collapse consecutive duplicates.
    let mut out: Vec<(u64, String)> = Vec::with_capacity(samples.len());
    for tr in samples {
        if let Some(last) = out.last() {
            if last.1 == tr.1 {
                continue;
            }
        }
        out.push(tr);
    }
    out
}

// ============================================================================
// ServerHandler
// ============================================================================

#[tool_handler]
impl ServerHandler for VerilogMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Circuit Scope Verilog MCP server (IEEE 1364, not SystemVerilog). Tools \
             cover parse/index, one-shot VCD simulation, VCD query, and an \
             interactive stepping debugger. Lifecycle for the debugger: \
             `sim_start` -> `sim_step` (repeat) -> `sim_end`. \
             Use `sim_list_sessions` to recover ids after a context loss, \
             `sim_state` to peek at a session, `sim_eval_many` for batched \
             watch lists, and `sim_source_excerpt` to fetch the source text \
             at a span returned by `sim_query_active` / `sim_driver_at`. \
             All file paths must be absolute. \
             **Clients must await each response before issuing the next** — \
             tool dispatches run concurrently, so pipelining `sim_step` \
             before `sim_start`'s response can race with session insertion. \
             Stdout is reserved for JSON-RPC; set `RUST_LOG=verilog_mcp=debug` \
             for stderr diagnostics."
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

// ============================================================================
// Entry
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    tracing::info!("verilog-mcp starting (stdio transport)");
    let service = VerilogMcpServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
