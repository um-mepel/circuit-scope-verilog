//! Long-lived simulator state held across Tauri invocations.
//!
//! The legacy [`crate::codegen::generate_vcd`] entry point owns a
//! [`crate::codegen::Simulator`] for the duration of a single call and throws
//! it away afterwards. The debugger, by contrast, needs to step the simulator
//! interactively — stop between two statements, hand the current time back to
//! the editor, then resume on the next user action. `SimSession` is that
//! resumable wrapper.
//!
//! The frontend holds a `SessionId` handle; the Tauri backend stores a
//! `Mutex<HashMap<SessionId, SimSession>>` (see `src-tauri/src/main.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::codegen::{SimConfig, StepMode, StepOutcome};
use crate::ir::IrProject;
use crate::source_map::{Span, SpanInfo};
use crate::trace::{BranchChoice, DriverEvent, TraceEntry};

/// Opaque handle surfaced to the frontend; allocated by
/// [`SessionRegistry::alloc_id`]. We keep it a plain `u64` so it can be marshaled
/// through Tauri's JSON bridge cheaply.
pub type SessionId = u64;

/// One live simulation with its backing VCD path and source-map snapshot.
///
/// The [`crate::codegen::Simulator`] itself is private; [`SimSession`] owns a
/// boxed handle behind [`SimRunner`] that exposes just the public step methods.
pub struct SimSession {
    runner: Box<dyn SimRunner + Send>,
    vcd_path: PathBuf,
    source_files: Vec<(u16, String)>,
    config: SimConfig,
}

impl SimSession {
    /// Build a new session for `project`/`config`. Elaborates the simulator,
    /// runs [`crate::codegen::Simulator::init_run`], and prepares the VCD path
    /// (but does not yet flush — call [`Self::flush_vcd`] after stepping).
    pub fn start(
        project: &IrProject,
        config: SimConfig,
        vcd_path: PathBuf,
    ) -> Result<Self, String> {
        let source_files: Vec<(u16, String)> = project
            .source_map
            .files()
            .map(|(id, path)| (id, path.to_string()))
            .collect();

        let runner = crate::codegen::new_stepping_simulator(project, &config)?;

        Ok(Self {
            runner,
            vcd_path,
            source_files,
            config,
        })
    }

    /// Advance the simulator one step in the requested mode. `clock_signal`
    /// (for [`StepMode::OneCycle`]) is passed through as-is — pass the fully
    /// hierarchical name (`top.clk`) if required by the flattened signal table.
    pub fn step(&mut self, mode: StepMode, clock_signal: Option<&str>) -> StepOutcome {
        self.runner.step_until(mode, clock_signal)
    }

    /// Persist the accumulated VCD buffer to disk, overwriting `vcd_path` each
    /// time. Cheap for small traces; for longer runs the caller may prefer to
    /// stream via [`Self::vcd_path`] directly from a `File`.
    pub fn flush_vcd(&mut self) -> std::io::Result<&Path> {
        fs::write(&self.vcd_path, self.runner.vcd_buffer())?;
        Ok(&self.vcd_path)
    }

    /// Full trace produced so far.
    pub fn trace(&self) -> &[TraceEntry] {
        self.runner.trace()
    }

    /// Spans "active at" `t_fs`, deduplicated so the editor overlay shows each
    /// source range at most once.
    ///
    /// For an exact trace hit (typical after a `sim_step`) we return all spans
    /// that fired at that femtosecond. For an arbitrary scrub time (from the
    /// waveform cursor, which almost never lands on the exact simulator clock)
    /// we fall back to the most recent entry at or before `t_fs` and return
    /// every span that shares that time — i.e. "the last instant of source
    /// activity at or before the cursor".
    pub fn active_spans_at(&self, t_fs: u64) -> Vec<Span> {
        let trace = self.runner.trace();
        let mut slice = crate::trace::trace_at(trace, t_fs);
        if slice.is_empty() {
            if let Some(latest) = crate::trace::trace_latest_at_or_before(trace, t_fs) {
                slice = crate::trace::trace_at(trace, latest.time_fs);
            }
        }
        let mut out: Vec<Span> = Vec::with_capacity(slice.len());
        for e in slice {
            if !out.contains(&e.span) {
                out.push(e.span);
            }
        }
        out
    }

    /// Resolve the current trace's spans at `t_fs` to full [`SpanInfo`] using
    /// the project's [`crate::SourceMap`]. Pass the same `project` that was
    /// used in [`Self::start`] (the session does not retain it).
    pub fn active_span_infos_at(&self, project: &IrProject, t_fs: u64) -> Vec<SpanInfo> {
        self.active_spans_at(t_fs)
            .into_iter()
            .filter_map(|s| project.source_map.resolve(s))
            .collect()
    }

    pub fn vcd_path(&self) -> &Path {
        &self.vcd_path
    }

    pub fn source_files(&self) -> &[(u16, String)] {
        &self.source_files
    }

    pub fn config(&self) -> &SimConfig {
        &self.config
    }

    pub fn current_time_fs(&self) -> u64 {
        self.runner.current_time_fs()
    }

    /// Look up the current value of a signal by its hierarchical name. See
    /// [`SimRunner::eval_signal`].
    pub fn eval_signal(&self, name: &str) -> Option<(i64, usize)> {
        self.runner.eval_signal(name)
    }

    /// Value lookup that tolerates every reasonable form of a dotted
    /// hierarchical name (same transformations as [`Self::driver_at`]).
    /// Returns `(value, width, resolved_name)` when any candidate matches.
    ///
    /// Use this — not [`Self::eval_signal`] — for hover tooltips where the
    /// input could be an unqualified identifier in a submodule file, a
    /// top-scope port (`TestBench.x`), or a fully canonical flattened name
    /// (`u1__x`).
    pub fn eval_signal_resolved(&self, name: &str) -> Option<(i64, usize, String)> {
        let (_, resolved) = self.resolve_signal_idx(name)?;
        let (v, w) = self.runner.eval_signal(&resolved)?;
        Some((v, w, resolved))
    }

    /// True when at least one [`DriverEvent`] fires for `signal_name` at
    /// exactly `t_fs`. Used by the editor hover tooltip to tag a variable
    /// as "transitioning" when the pinned simulator time lands on a write.
    ///
    /// Signal name resolution follows the same rules as [`Self::driver_at`]:
    /// both dotted (`TestBench.u.x`) and already-canonical (`u__x`) forms
    /// work.
    pub fn is_transitioning_at(&self, signal_name: &str, t_fs: u64) -> bool {
        self
            .transition_info_at(signal_name, t_fs)
            .map(|(from, to)| from != to)
            .unwrap_or(false)
    }

    /// When a [`DriverEvent`] fires for `signal_name` at exactly `t_fs`,
    /// return `Some((prev_value, new_value))` — i.e. the "from → to" of
    /// the transition. The `prev_value` is the value of the most recent
    /// driver event before `t_fs`, or `0` when nothing has driven the
    /// signal yet.
    ///
    /// Returns `None` when the signal does not resolve or when no driver
    /// event lands on `t_fs`.
    pub fn transition_info_at(&self, signal_name: &str, t_fs: u64) -> Option<(i64, i64)> {
        let (idx, _) = self.resolve_signal_idx(signal_name)?;
        let events = self.runner.driver_events();
        // Find the event exactly at t_fs (the "to") and the latest one
        // strictly before it (the "from"). We scan from the end because
        // driver events are appended in simulated-time order.
        let mut to_val: Option<i64> = None;
        let mut from_val: i64 = 0;
        for e in events.iter().rev() {
            if e.signal_idx != idx {
                continue;
            }
            if e.time_fs == t_fs && to_val.is_none() {
                to_val = Some(e.value);
            } else if e.time_fs < t_fs {
                from_val = e.value;
                break;
            }
        }
        to_val.map(|to| (from_val, to))
    }

    /// Historical value lookup at a pinned time `t_fs`.
    ///
    /// Uses the driver-event stream (latest event at-or-before `t_fs`) instead
    /// of the live simulator register file so hover queries stay time-synced
    /// with the pinned waveform cursor.
    pub fn eval_signal_at_or_before(
        &self,
        name: &str,
        t_fs: u64,
    ) -> Option<(i64, usize, String)> {
        let (idx, resolved) = self.resolve_signal_idx(name)?;
        let width = self.runner.eval_signal(&resolved).map(|(_, w)| w).unwrap_or(1);
        let ev = crate::trace::driver_latest_at_or_before(self.runner.driver_events(), idx, t_fs)?;
        Some((ev.value, width, resolved))
    }

    /// Resolve the signal index for a hierarchical name, trying every
    /// reasonable reinterpretation of a dotted path.
    ///
    /// The frontend sends names like `TestBench7.ffc.Add` (from the VCD scope
    /// tree) or `TestBench7.HEX4` (a top-module port). The simulator's
    /// flattened table instead uses underscore-joined names and, crucially,
    /// drops the top-module prefix — so `HEX4` is stored as `HEX4` and
    /// `ffc.Add` as `ffc__Add`.
    ///
    /// We enumerate every suffix of the dotted name, for each suffix produce
    /// both the dotted and the `__`-joined form, and prefer exact matches
    /// over a last-resort suffix search. This handles:
    ///   * top-module port sent with scope: `TestBench7.HEX4` → `HEX4`,
    ///   * submodule signal: `TestBench7.ffc.Add` → `ffc__Add`,
    ///   * already-canonical name: `ffc__Add` → `ffc__Add`,
    ///   * a VCD viewer that omits the top scope: `HEX4` → `HEX4`.
    fn resolve_signal_idx(&self, name: &str) -> Option<(u32, String)> {
        let names = self.runner.signal_names();
        let parts: Vec<&str> = name.split('.').collect();
        let mut candidates: Vec<String> = Vec::with_capacity(parts.len() * 2);
        for i in 0..parts.len() {
            let dotted = parts[i..].join(".");
            let underscored = parts[i..].join("__");
            if !candidates.iter().any(|c| c == &dotted) {
                candidates.push(dotted);
            }
            if !candidates.iter().any(|c| c == &underscored) {
                candidates.push(underscored);
            }
        }

        // Preferred: exact match on any candidate. Iterates the shortest
        // stripping (full name) first so we pick the most specific hit.
        for cand in &candidates {
            if let Some((i, n)) =
                names.iter().enumerate().find(|(_, n)| n.as_str() == cand.as_str())
            {
                return Some((i as u32, n.clone()));
            }
        }

        // Last resort: longest-suffix match. Only activates when the VCD
        // surfaces a differently-rooted path (e.g. wrapper renaming).
        for cand in &candidates {
            if let Some((i, n)) = names
                .iter()
                .enumerate()
                .find(|(_, n)| n.ends_with(cand.as_str()))
            {
                return Some((i as u32, n.clone()));
            }
        }
        None
    }

    /// Resolve the driver of `signal_name` at (or before) `t_fs`.
    ///
    /// Returns the originating statement span and, when available, a
    /// [`BranchChoice`] indicating which arm of a top-level ternary rhs the
    /// simulator took. Sub-span resolution (which substring inside the stmt
    /// to highlight) is deferred to the frontend, which already has the
    /// file's source text loaded.
    ///
    /// `project` must be the same IR project used in [`Self::start`]; the
    /// [`crate::SourceMap`] it carries is used to resolve file paths.
    pub fn driver_at(
        &self,
        project: &IrProject,
        signal_name: &str,
        t_fs: u64,
    ) -> Option<DriverQueryResult> {
        let (idx0, resolved_name) = self.resolve_signal_idx(signal_name)?;

        // Follow up to `MAX_HOPS` port-glue aliases. At each hop we try:
        //   1. runtime `DriverEvent` for that signal's index, then
        //   2. static-driver fallback.
        // If neither hits, hop along the alias to the upstream signal
        // (typically an instance's internal wire) and try again. This lets
        // "Jump to Driver" reach the real source when the nominal signal is
        // only written via instance-port glue (whose dummy spans we drop).
        const MAX_HOPS: usize = 16;
        let mut current_name = resolved_name.clone();
        let mut current_idx = idx0;
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        visited.insert(current_name.clone());

        for _hop in 0..MAX_HOPS {
            if let Some(ev) = crate::trace::driver_latest_at_or_before(
                self.runner.driver_events(),
                current_idx,
                t_fs,
            ) {
                if let Some(stmt_info) = project.source_map.resolve(ev.stmt_span) {
                    let lhs_leaf = resolved_name
                        .rsplit_once("__")
                        .map(|(_, leaf)| leaf)
                        .unwrap_or(resolved_name.as_str());
                    let runtime_matches = span_assigns_to_signal(
                        &stmt_info.path,
                        ev.stmt_span.start,
                        ev.stmt_span.end,
                        lhs_leaf,
                    );
                    if runtime_matches {
                        return Some(DriverQueryResult {
                            resolved_signal: resolved_name,
                            file_id: ev.stmt_span.file_id,
                            path: stmt_info.path,
                            stmt_span: ev.stmt_span,
                            branch: ev.branch,
                        });
                    }
                }
            }
            if let Some(span) = self.runner.static_driver(&current_name) {
                if let Some(stmt_info) = project.source_map.resolve(span) {
                    let lhs_leaf = resolved_name
                        .rsplit_once("__")
                        .map(|(_, leaf)| leaf)
                        .unwrap_or(resolved_name.as_str());
                    let static_matches = span_assigns_to_signal(
                        &stmt_info.path,
                        span.start,
                        span.end,
                        lhs_leaf,
                    );
                    if static_matches {
                        return Some(DriverQueryResult {
                            resolved_signal: resolved_name,
                            file_id: span.file_id,
                            path: stmt_info.path,
                            stmt_span: span,
                            branch: None,
                        });
                    }
                }
            }

            let next = self.runner.port_alias(&current_name)?;
            if !visited.insert(next.clone()) {
                return None;
            }
            let next_idx = self.runner.signal_index_of(&next)?;
            current_name = next;
            current_idx = next_idx;
        }
        None
    }

    /// Best-effort fallback when branch metadata is unavailable: find an RHS
    /// identifier whose signal also has a driver event at exactly `t_fs`.
    pub fn driving_token_span_at(
        &self,
        signal_name: &str,
        t_fs: u64,
        stmt_span: Span,
        path: &str,
    ) -> Option<TextSpan> {
        let (target_idx, resolved_target) = self.resolve_signal_idx(signal_name)?;
        let events = self.runner.driver_events();
        let mut changed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for e in events.iter().filter(|e| e.time_fs == t_fs && e.signal_idx != target_idx) {
            let prev = crate::trace::driver_latest_at_or_before(events, e.signal_idx, t_fs.saturating_sub(1))
                .map(|p| p.value)
                .unwrap_or(0);
            if prev != e.value {
                changed.insert(e.signal_idx);
            }
        }
        if changed.is_empty() {
            return None;
        }

        let text = std::fs::read_to_string(path).ok()?;
        let start = stmt_span.start as usize;
        let end = stmt_span.end as usize;
        if start >= end || end > text.len() {
            return None;
        }
        let stmt = &text[start..end];
        let eq = stmt.find('=')?;
        let rhs = &stmt[(eq + 1)..];
        let rhs_abs = start + eq + 1;
        let scope_prefix = resolved_target.rsplit_once("__").map(|(p, _)| p.to_string());
        let lhs_leaf = resolved_target
            .rsplit_once("__")
            .map(|(_, leaf)| leaf.to_string())
            .unwrap_or(resolved_target.clone());

        let bytes = rhs.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if !(c == '_' || c.is_ascii_alphabetic()) {
                i += 1;
                continue;
            }
            let tok_start = i;
            i += 1;
            while i < bytes.len() {
                let cc = bytes[i] as char;
                if cc == '_' || cc == '$' || cc.is_ascii_alphanumeric() {
                    i += 1;
                } else {
                    break;
                }
            }
            let tok = &rhs[tok_start..i];
            if is_verilog_keyword(tok) || tok == lhs_leaf {
                continue;
            }

            let mut candidates: Vec<String> = Vec::new();
            if let Some(prefix) = &scope_prefix {
                candidates.push(format!("{prefix}__{tok}"));
            }
            candidates.push(tok.to_string());

            for cand in candidates {
                if let Some((idx, _)) = self.resolve_signal_idx(&cand) {
                    if changed.contains(&idx) {
                        // Expand token span to include immediate bit/part-select
                        // suffixes like `foo[W-1]` so the highlight points at the
                        // full driving operand, not just the base identifier.
                        let mut j = i;
                        while j < bytes.len() {
                            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                                j += 1;
                            }
                            if j >= bytes.len() || bytes[j] as char != '[' {
                                break;
                            }
                            let mut depth = 0i32;
                            while j < bytes.len() {
                                let cj = bytes[j] as char;
                                if cj == '[' {
                                    depth += 1;
                                } else if cj == ']' {
                                    depth -= 1;
                                    if depth == 0 {
                                        j += 1;
                                        break;
                                    }
                                }
                                j += 1;
                            }
                            if depth != 0 {
                                // Unbalanced bracket in slice; keep base token only.
                                j = i;
                                break;
                            }
                        }
                        let abs_a = rhs_abs + tok_start;
                        let abs_b = rhs_abs + j.max(i);
                        return Some(TextSpan {
                            start: abs_a as u32,
                            end: abs_b as u32,
                        });
                    }
                }
            }
        }
        None
    }
}

fn span_assigns_to_signal(path: &str, start: u32, end: u32, lhs_leaf: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let a = start as usize;
    let b = end as usize;
    if a >= b || b > text.len() {
        return true;
    }
    let slice = &text[a..b];
    let eq = slice.find('=').unwrap_or(slice.len());
    let lhs = slice[..eq].trim();
    // Require identifier-boundary matches in lhs text to avoid false hits.
    lhs.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .any(|tok| tok == lhs_leaf)
}

/// Driver-provenance response used by `sim_driver_at`.
#[derive(Debug, Clone)]
pub struct DriverQueryResult {
    pub resolved_signal: String,
    pub file_id: u16,
    pub path: String,
    pub stmt_span: Span,
    pub branch: Option<BranchChoice>,
}

#[derive(Debug, Clone, Copy)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

fn is_verilog_keyword(tok: &str) -> bool {
    matches!(
        tok,
        "if"
            | "else"
            | "begin"
            | "end"
            | "assign"
            | "always"
            | "initial"
            | "posedge"
            | "negedge"
            | "case"
            | "endcase"
            | "for"
            | "while"
            | "repeat"
            | "wire"
            | "reg"
            | "logic"
            | "input"
            | "output"
            | "inout"
            | "module"
            | "endmodule"
            | "parameter"
            | "localparam"
            | "integer"
            | "signed"
            | "unsigned"
    )
}

/// Trait-object shim so `SimSession` does not need to name the private
/// `Simulator` type.
pub trait SimRunner {
    fn step_until(&mut self, mode: StepMode, clock_signal: Option<&str>) -> StepOutcome;
    fn vcd_buffer(&self) -> &str;
    fn trace(&self) -> &[TraceEntry];
    /// Per-write driver provenance (see [`DriverEvent`]). Used by the
    /// "Jump to Driver" debugger feature.
    fn driver_events(&self) -> &[DriverEvent];
    /// Canonical list of flattened hierarchical signal names. Indexes into
    /// this slice match `DriverEvent::signal_idx`.
    fn signal_names(&self) -> &[String];
    /// Statically-discovered driver span for a flattened signal name, or
    /// `None` if the signal has no source-level writer in the elaborated
    /// design (pure input, undriven net, …). Populated once at simulator
    /// construction from every `assign`/procedural-assign in the flattened
    /// IR, used as a fallback when the runtime driver trace has no event
    /// for this signal yet. See [`SimSession::driver_at`].
    fn static_driver(&self, _signal: &str) -> Option<Span> {
        None
    }
    /// One-hop alias for instance-port glue: `parent = Ident(child)` produces
    /// `parent → child`. Used by [`SimSession::driver_at`] to walk from a
    /// synthesized-glue top-level port to the real writing statement
    /// (typically inside the instantiated submodule). `None` means this
    /// signal has no alias; the caller should then treat it as undriven.
    fn port_alias(&self, _signal: &str) -> Option<String> {
        None
    }
    /// Index of a flattened signal name in [`Self::signal_names`], or `None`
    /// when the name is unknown. Used when walking port aliases so we can
    /// re-key the runtime `driver_events` lookup on the aliased signal
    /// without re-running the full dotted-name resolution.
    fn signal_index_of(&self, _signal: &str) -> Option<u32> {
        None
    }
    fn current_time_fs(&self) -> u64;
    /// Read the current value of a flattened signal (e.g. `top.u1.count`).
    /// `None` when the signal has not been registered in the simulator (bad
    /// identifier or not-yet-driven memory cell).
    fn eval_signal(&self, name: &str) -> Option<(i64, usize)>;
}

/// Thread-safe registry of live sessions. Put this behind a `Mutex` in Tauri
/// state; the IDs are monotonically increasing and never recycled within a
/// single process.
#[derive(Default)]
pub struct SessionRegistry {
    next: u64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc_id(&mut self) -> SessionId {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }
}

