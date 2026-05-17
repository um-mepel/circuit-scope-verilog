//! Tauri bridge for the time-synced source debugger (Phase 6).
//!
//! These commands expose [`verilog_core::SimSession`] to the frontend. The
//! registry behind `DebuggerSessions` holds `(SimSession, IrProject)` keyed by
//! a monotonically increasing `u64` session id so the frontend can refer to a
//! particular debug run without juggling Rust lifetimes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::State;
use verilog_core::{
    build_ir_for_root, find_top_module, optimize_project, BranchChoice,
    SessionId, SessionRegistry, SimConfig, SimSession, StepMode,
    StepOutcome,
};
use verilog_core::IrProject;

/// Live debugger sessions keyed by [`SessionId`].
///
/// One `DebuggerSessions` instance is [`tauri::Manager::manage`]-registered at
/// startup; the mutex guards the (registry, sessions) pair so allocating a new
/// id and inserting its session happen atomically.
#[derive(Default)]
pub struct DebuggerSessions {
    inner: Mutex<DebuggerSessionsInner>,
}

#[derive(Default)]
struct DebuggerSessionsInner {
    registry: SessionRegistry,
    sessions: HashMap<SessionId, SessionEntry>,
}

struct SessionEntry {
    session: SimSession,
    project: IrProject,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFileEntry {
    pub id: u16,
    pub path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimStartResult {
    pub session_id: u64,
    pub vcd_path: String,
    pub source_files: Vec<SourceFileEntry>,
    pub time_fs: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSpan {
    pub file_id: u16,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub line_start: u32,
    pub col_start: u32,
    pub line_end: u32,
    pub col_end: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub time_fs: u64,
    pub active_spans: Vec<ActiveSpan>,
    pub ran_statements: usize,
    pub done: bool,
}

// Breakpoint wire types (BreakpointDto / BreakpointHitDto) were removed for
// the 0.3.0 release. Restore from git history when the breakpoint feature is
// re-enabled in verilog-core.

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    pub decimal: String,
    pub hex: String,
    pub binary: String,
    pub width: usize,
    pub resolved_name: String,
    /// True when a [`DriverEvent`] fires for this signal at exactly the
    /// requested `time_fs`. The editor surfaces this as a "transitioning"
    /// badge in the hover tooltip.
    pub transitioning: bool,
    /// Previous value of the signal, rendered the same way as the current
    /// value. Populated only when `transitioning == true` — this is the
    /// "from" side of the transition that the editor shows as
    /// `prev → curr` in the hover tooltip.
    pub prev_decimal: Option<String>,
    pub prev_hex: Option<String>,
    pub prev_binary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimStartArgs {
    pub project_root: String,
    pub top_module: Option<String>,
    pub num_cycles: Option<usize>,
    pub vcd_filename: Option<String>,
}

fn parse_mode(raw: &str) -> Result<StepMode, String> {
    match raw {
        "statement" | "OneStatement" => Ok(StepMode::OneStatement),
        "tick" | "OneTick" => Ok(StepMode::OneTick),
        "cycle" | "OneCycle" => Ok(StepMode::OneCycle),
        "run" | "ToEnd" => Ok(StepMode::ToEnd),
        other => Err(format!("unknown step mode: {other}")),
    }
}

fn to_active_span(
    info: verilog_core::SpanInfo,
) -> ActiveSpan {
    ActiveSpan {
        file_id: info.file_id,
        path: info.path,
        start: info.start,
        end: info.end,
        line_start: info.line_start,
        col_start: info.col_start,
        line_end: info.line_end,
        col_end: info.col_end,
    }
}

fn collect_active(session: &SimSession, project: &IrProject, t_fs: u64) -> Vec<ActiveSpan> {
    session
        .active_span_infos_at(project, t_fs)
        .into_iter()
        .map(to_active_span)
        .collect()
}

/// Start a new debugger session rooted at `project_root`. Elaborates, optimises,
/// picks the top module (auto-detecting when not specified), and runs
/// [`SimSession::start`] which calls [`Simulator::init_run`] internally so the
/// VCD header is ready before the first step.
#[tauri::command]
pub async fn sim_start(
    sessions: State<'_, DebuggerSessions>,
    args: SimStartArgs,
) -> Result<SimStartResult, String> {
    let root = PathBuf::from(&args.project_root);
    let mut project = build_ir_for_root(&root).map_err(|e| e.to_string())?;
    optimize_project(&mut project);

    let top = match args.top_module {
        Some(t) => t,
        None => find_top_module(&project)?,
    };

    let cycles = args.num_cycles.unwrap_or(64);
    let config = SimConfig {
        top_module: top,
        num_cycles: cycles,
        ..Default::default()
    };

    let vcd_filename = args.vcd_filename.unwrap_or_else(|| "debug.vcd".into());
    if vcd_filename.contains('/') || vcd_filename.contains('\\') {
        return Err("vcdFilename must be a file name only".into());
    }
    let vcd_path = root.join(&vcd_filename);

    let mut session = SimSession::start(&project, config, vcd_path.clone())?;
    // Ensure the VCD header is on disk before the frontend opens it via vcd_open.
    session.flush_vcd().map_err(|e| e.to_string())?;

    let source_files = project
        .source_map
        .files()
        .map(|(id, path)| SourceFileEntry {
            id,
            path: path.to_string(),
        })
        .collect();
    let time_fs = session.current_time_fs();

    let mut state = sessions.inner.lock().map_err(|e| e.to_string())?;
    let id = state.registry.alloc_id();
    state.sessions.insert(
        id,
        SessionEntry {
            session,
            project,
        },
    );

    Ok(SimStartResult {
        session_id: id,
        vcd_path: vcd_path.to_string_lossy().to_string(),
        source_files,
        time_fs,
    })
}

/// Advance the session in the requested step mode.
///
/// Breakpoints were removed for 0.3.0; this command simply steps the
/// underlying [`SimSession`] and returns the active spans. Restore the
/// breakpoint plumbing from git history when re-enabling.
#[tauri::command]
pub async fn sim_step(
    sessions: State<'_, DebuggerSessions>,
    session_id: u64,
    mode: String,
    clock_signal: Option<String>,
) -> Result<StepResult, String> {
    let mode = parse_mode(&mode)?;
    let mut state = sessions.inner.lock().map_err(|e| e.to_string())?;
    let entry = state
        .sessions
        .get_mut(&session_id)
        .ok_or_else(|| format!("unknown sessionId: {session_id}"))?;

    let outcome = entry.session.step(mode, clock_signal.as_deref());
    entry.session.flush_vcd().map_err(|e| e.to_string())?;
    let t_fs = entry.session.current_time_fs();
    let active = collect_active(&entry.session, &entry.project, t_fs);
    let (ran, done) = match outcome {
        StepOutcome::Advanced { ran_statements, .. } => (ran_statements, false),
        StepOutcome::End => (0, true),
    };
    Ok(StepResult {
        time_fs: t_fs,
        active_spans: active,
        ran_statements: ran,
        done,
    })
}

/// Spans that fired at `time_fs`, regardless of current simulator time. Used
/// by the editor overlay to re-render after a waveform scrub without running
/// the simulator.
#[tauri::command]
pub async fn sim_query_active(
    sessions: State<'_, DebuggerSessions>,
    session_id: u64,
    time_fs: u64,
) -> Result<Vec<ActiveSpan>, String> {
    let state = sessions.inner.lock().map_err(|e| e.to_string())?;
    let entry = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| format!("unknown sessionId: {session_id}"))?;
    Ok(collect_active(&entry.session, &entry.project, time_fs))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimEvalArgs {
    pub session_id: u64,
    pub identifier: String,
    /// Reserved for future scope-aware lookup; currently used only to pick the
    /// enclosing module by matching the file path against
    /// [`IrModule::path`].
    pub file_path: Option<String>,
    pub time_fs: Option<u64>,
    #[allow(dead_code)]
    pub byte_pos: Option<u32>,
}

/// Look up the live value of `identifier` in the simulator. Resolution order:
/// 1. As-typed (supports hierarchical `top.u1.x`).
/// 2. `"{top_module}.{identifier}"` — for bare identifiers inside the top.
/// 3. `"{module_containing_file_path}.{identifier}"` — for identifiers inside a non-top module.
#[tauri::command]
pub async fn sim_eval(
    sessions: State<'_, DebuggerSessions>,
    args: SimEvalArgs,
) -> Result<EvalResult, String> {
    let state = sessions.inner.lock().map_err(|e| e.to_string())?;
    let entry = state
        .sessions
        .get(&args.session_id)
        .ok_or_else(|| format!("unknown sessionId: {}", args.session_id))?;

    // Build every plausible dotted form of the identifier: as-typed,
    // prefixed with the top module, and — when we know what file the
    // hover happened in — prefixed with every module declared in that
    // file (so an unqualified name inside a submodule still resolves).
    // `eval_signal_resolved` then handles dot↔underscore canonicalisation
    // and suffix-matching against the flattened signal table.
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
            let render = |v: i64| -> (String, String, String) {
                let u = (v as u64) & (mask as u64);
                (
                    u.to_string(),
                    format!("0x{:X}", u),
                    format!("0b{:0width$b}", u, width = width.max(1)),
                )
            };
            let unsigned = (val as u64) & (mask as u64);
            let transition_raw = match args.time_fs {
                Some(t) => entry.session.transition_info_at(&resolved, t),
                None => None,
            };
            let transition = transition_raw.filter(|(from, to)| from != to);
            let (prev_decimal, prev_hex, prev_binary) = match transition {
                Some((from, _)) => {
                    let (d, h, b) = render(from);
                    (Some(d), Some(h), Some(b))
                }
                None => (None, None, None),
            };
            return Ok(EvalResult {
                decimal: unsigned.to_string(),
                hex: format!("0x{:X}", unsigned),
                binary: format!("0b{:0width$b}", unsigned, width = width.max(1)),
                width,
                resolved_name: resolved,
                transitioning: transition.is_some(),
                prev_decimal,
                prev_hex,
                prev_binary,
            });
        }
    }

    Err(format!(
        "identifier '{}' not found; tried: {:?}",
        args.identifier, candidates
    ))
}

/// Pure span lookup at `time_fs`: used by the waveform when the user scrubs
/// the cursor without stepping the simulator. Alias for [`sim_query_active`]
/// today; kept as its own command per the plan so we can add semantic
/// differences later (e.g. batching).
#[tauri::command]
pub async fn sim_seek(
    sessions: State<'_, DebuggerSessions>,
    session_id: u64,
    time_fs: u64,
) -> Result<Vec<ActiveSpan>, String> {
    sim_query_active(sessions, session_id, time_fs).await
}

/// Drop the session and its simulator, and best-effort delete the VCD file
/// created for this session so it doesn't pile up in the project directory.
/// Subsequent calls with the same `sessionId` will return an "unknown
/// sessionId" error.
#[tauri::command]
pub async fn sim_end(
    sessions: State<'_, DebuggerSessions>,
    session_id: u64,
) -> Result<(), String> {
    let mut state = sessions.inner.lock().map_err(|e| e.to_string())?;
    if let Some(entry) = state.sessions.remove(&session_id) {
        let path = entry.session.vcd_path().to_path_buf();
        // Silently ignore — the file may already be gone or still held open
        // by the VCD viewer on some platforms. Frontend closes the viewer
        // before invoking this command to minimise that race.
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverAtResult {
    /// Fully-resolved hierarchical signal name the backend matched against.
    pub resolved_signal: String,
    pub file_id: u16,
    pub path: String,
    /// Whole statement span (assign/always body) in byte offsets.
    pub stmt_start: u32,
    pub stmt_end: u32,
    /// Chosen branch (ternary arm) narrowed from `stmt_*`. `None` when not
    /// a ternary or when sub-span resolution failed — frontend should fall
    /// back to the statement span.
    pub branch_start: Option<u32>,
    pub branch_end: Option<u32>,
    /// Condition sub-span (the `cond` of a `cond ? a : b`), paired with
    /// `branch_*` when resolved.
    pub cond_start: Option<u32>,
    pub cond_end: Option<u32>,
}

/// Resolve the driving block for `signal` at `time_fs`.
///
/// Triggered by the waveform's explicit "Jump to Driver" button after the
/// user focused a trace row and pinned a time. Returns the statement span
/// plus, when the rhs is a top-level ternary, the sub-span of the chosen
/// arm (trimmed of surrounding whitespace).
#[tauri::command]
pub async fn sim_driver_at(
    sessions: State<'_, DebuggerSessions>,
    session_id: u64,
    signal: String,
    time_fs: u64,
) -> Result<DriverAtResult, String> {
    let state = sessions.inner.lock().map_err(|e| e.to_string())?;
    let entry = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| format!("unknown sessionId: {session_id}"))?;

    let res = entry
        .session
        .driver_at(&entry.project, &signal, time_fs)
        .ok_or_else(|| {
            // `driver_at` returns `None` only when the hierarchical signal
            // name cannot be resolved in the flattened simulator table.
            // Any driven signal has a static-driver fallback, so a miss
            // here is an unknown-signal case, not a "no writes yet" case.
            format!("could not resolve signal '{signal}' in the simulator; it may be outside the elaborated top module")
        })?;

    let lhs_leaf = res
        .resolved_signal
        .rsplit_once("__")
        .map(|(_, leaf)| leaf.to_string())
        .unwrap_or_else(|| res.resolved_signal.clone());
    let stmt_valid = span_assigns_to_lhs(&res.path, res.stmt_span.start, res.stmt_span.end, &lhs_leaf);
    let direct_lhs_stmt = find_assign_stmt_for_lhs(&res.path, &lhs_leaf)?;
    let (stmt_start, stmt_end) =
        if let Some((a, b)) = direct_lhs_stmt {
            (a, b)
        } else if stmt_valid {
            (res.stmt_span.start, res.stmt_span.end)
        } else if let Some((a, b)) = find_assign_stmt_for_lhs(&res.path, &lhs_leaf)? {
            (a, b)
        } else {
            (res.stmt_span.start, res.stmt_span.end)
        };

    // Resolve sub-spans by reading the source file and chopping the
    // top-level `?`/`:` of the ternary rhs. Errors/mismatches fall back to
    // statement-only highlighting so the UI still works.
    let (cond_span, mut branch_span) = match res.branch {
        Some(choice) => ternary_sub_spans(&res.path, stmt_start, stmt_end, choice)
            .unwrap_or((None, None)),
        None => (None, None),
    };
    if branch_span.is_none() && res.branch.is_none() {
        // For non-ternary assignments, highlight the full RHS expression
        // (e.g. `A[W-1] ^ N_TC[W-1]`) instead of a single operand token.
        branch_span = rhs_expr_span(&res.path, stmt_start, stmt_end).ok().flatten();
    }
    if branch_span.is_none() {
        let repaired_stmt_span = verilog_core::Span::new(res.stmt_span.file_id, stmt_start, stmt_end);
        branch_span = entry
            .session
            .driving_token_span_at(&signal, time_fs, repaired_stmt_span, &res.path)
            .map(|s| (s.start, s.end));
    }
    if let Some((a, b)) = branch_span {
        // Keep sub-highlights inside the chosen statement; stale/out-of-scope
        // spans are dropped to avoid highlighting unrelated blocks.
        if a < stmt_start || b > stmt_end || b <= a {
            branch_span = None;
        }
    }
    Ok(DriverAtResult {
        resolved_signal: res.resolved_signal,
        file_id: res.file_id,
        path: res.path,
        stmt_start,
        stmt_end,
        branch_start: branch_span.map(|(a, _)| a),
        branch_end: branch_span.map(|(_, b)| b),
        cond_start: cond_span.map(|(a, _)| a),
        cond_end: cond_span.map(|(_, b)| b),
    })
}

/// Chop a `lhs = cond ? a : b ;` statement at the top-level `?`/`:` and
/// return `(cond, chosen_arm)` as absolute `(start, end)` byte offsets.
///
/// Tracks `()`/`[]`/`{}` depth so nested ternaries and part-selects don't
/// confuse the split. Returns `Ok((None, None))` when the statement does
/// not match a ternary shape.
fn ternary_sub_spans(
    path: &str,
    stmt_start: u32,
    stmt_end: u32,
    choice: BranchChoice,
) -> Result<(Option<(u32, u32)>, Option<(u32, u32)>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let start = stmt_start as usize;
    let end = stmt_end as usize;
    if start >= text.len() || end > text.len() || end <= start {
        return Ok((None, None));
    }
    let slice = &text[start..end];
    let Some(eq) = find_top_level(slice, |c| c == '=') else {
        return Ok((None, None));
    };
    let rhs_start = eq + 1;
    let Some(q) = find_top_level(&slice[rhs_start..], |c| c == '?').map(|p| p + rhs_start) else {
        return Ok((None, None));
    };
    let Some(colon) = find_top_level(&slice[q + 1..], |c| c == ':').map(|p| p + q + 1) else {
        return Ok((None, None));
    };
    let tail = slice[colon + 1..]
        .bytes()
        .position(|b| b == b';')
        .map(|p| p + colon + 1)
        .unwrap_or(slice.len());

    let bytes = slice.as_bytes();
    let trim = |lo: usize, hi: usize| -> (usize, usize) {
        let mut a = lo;
        let mut b = hi;
        while a < b && bytes[a].is_ascii_whitespace() {
            a += 1;
        }
        while b > a && bytes[b - 1].is_ascii_whitespace() {
            b -= 1;
        }
        (a, b)
    };
    let (ca, cb) = trim(rhs_start, q);
    let cond_span = Some(((start + ca) as u32, (start + cb) as u32));
    let (ba, bb) = match choice {
        BranchChoice::Then => trim(q + 1, colon),
        BranchChoice::Else => trim(colon + 1, tail),
    };
    let branch_span = Some(((start + ba) as u32, (start + bb) as u32));
    Ok((cond_span, branch_span))
}

fn find_top_level<F: Fn(char) -> bool>(s: &str, pred: F) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        match c {
            '(' => paren += 1,
            ')' => paren = (paren - 1).max(0),
            '[' => bracket += 1,
            ']' => bracket = (bracket - 1).max(0),
            '{' => brace += 1,
            '}' => brace = (brace - 1).max(0),
            _ => {}
        }
        if paren == 0 && bracket == 0 && brace == 0 && pred(c) {
            return Some(i);
        }
    }
    None
}

fn span_assigns_to_lhs(path: &str, stmt_start: u32, stmt_end: u32, lhs_leaf: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let start = stmt_start as usize;
    let end = stmt_end as usize;
    if start >= text.len() || end > text.len() || end <= start {
        return false;
    }
    let slice = &text[start..end];
    let contains_lhs = slice.contains(lhs_leaf);
    let Some(eq) = find_top_level(slice, |c| c == '=') else {
        return false;
    };
    if !contains_lhs {
        return false;
    }
    let lhs = slice[..eq].trim();
    let ok = lhs
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .any(|t| t == lhs_leaf);
    ok
}

fn find_assign_stmt_for_lhs(path: &str, lhs_leaf: &str) -> Result<Option<(u32, u32)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut cursor = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if !(t.starts_with("assign ") || t.contains("<=") || t.contains('=')) {
            cursor += line.len() + 1;
            continue;
        }
        if !t.contains(lhs_leaf) || !t.contains('=') {
            cursor += line.len() + 1;
            continue;
        }
        // Restrict to clear LHS hits (`assign lhs =` or `lhs <=`).
        let lhs_hit = t.starts_with(&format!("assign {lhs_leaf} "))
            || t.starts_with(&format!("assign {lhs_leaf}="))
            || t.starts_with(&format!("{lhs_leaf} <="))
            || t.starts_with(&format!("{lhs_leaf}<="))
            || t.starts_with(&format!("{lhs_leaf} ="))
            || t.starts_with(&format!("{lhs_leaf}="));
        if !lhs_hit {
            cursor += line.len() + 1;
            continue;
        }
        let start = cursor as u32;
        let end = (cursor + line.len()) as u32;
        return Ok(Some((start, end)));
        
    }
    Ok(None)
}

/// Trimmed RHS of an assignment statement in absolute byte offsets.
///
/// Works for both blocking (`=`) and nonblocking (`<=`) forms because both
/// contain `=` and we cut from the first top-level `=` to the next `;`.
fn rhs_expr_span(path: &str, stmt_start: u32, stmt_end: u32) -> Result<Option<(u32, u32)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let start = stmt_start as usize;
    let end = stmt_end as usize;
    if start >= text.len() || end > text.len() || end <= start {
        return Ok(None);
    }
    let slice = &text[start..end];
    let Some(eq) = find_top_level(slice, |c| c == '=') else {
        return Ok(None);
    };
    let rhs_lo = eq + 1;
    let rhs_hi = slice[rhs_lo..]
        .bytes()
        .position(|b| b == b';')
        .map(|p| rhs_lo + p)
        .unwrap_or(slice.len());
    let bytes = slice.as_bytes();
    let mut a = rhs_lo;
    let mut b = rhs_hi;
    while a < b && bytes[a].is_ascii_whitespace() {
        a += 1;
    }
    while b > a && bytes[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    if b <= a {
        return Ok(None);
    }
    Ok(Some(((start + a) as u32, (start + b) as u32)))
}
