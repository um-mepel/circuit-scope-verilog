//! Execution trace: per-statement `(time, span)` records emitted by the simulator,
//! plus per-signal driver provenance records used by the "Jump to Driver"
//! debugger feature.
//!
//! The debugger frontend pulls slices of [`TraceEntry`] to highlight "what
//! source statements fired at time T" and to scrub the editor cursor in
//! lockstep with the waveform cursor.
//!
//! [`DriverEvent`] is a parallel stream emitted every time a signal is
//! assigned: it pins down which statement (and, for ternary rhs, which arm of
//! the conditional) is responsible for that value. See
//! [`crate::sim_session::SimSession::driver_at`] for the query API.

use crate::source_map::Span;

/// One record per `IrStmt` execution inside the tree-walking interpreter.
///
/// `time_fs` is the simulator's femtosecond clock at the moment the statement
/// fired (see [`crate::codegen::Simulator`]). Multiple entries may share the
/// same `time_fs` when many statements run within one scheduler tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraceEntry {
    pub time_fs: u64,
    pub span: Span,
}

impl TraceEntry {
    pub const fn new(time_fs: u64, span: Span) -> Self {
        Self { time_fs, span }
    }
}

/// Branch chosen for a ternary right-hand side at a specific write.
///
/// Captured when the simulator evaluates `lhs = cond ? then : else` so the
/// editor can highlight the exact arm that produced the value. `None` means
/// the rhs was not a conditional expression and only statement-level
/// highlighting is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BranchChoice {
    Then,
    Else,
}

/// Per-assignment provenance record.
///
/// `stmt_span` always points to the driving statement (continuous `assign` or
/// procedural assignment). `branch` is set when the rhs is an `IrExpr::Ternary`
/// so the frontend can chop out the taken arm from the source text. More
/// conditional forms (nested ternaries, case defaults, …) can be added later
/// without breaking the wire format — consumers are expected to treat
/// unknown branch variants as "statement-only".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DriverEvent {
    pub time_fs: u64,
    /// Index into `SimSession::signal_names` so we don't store full names per
    /// event. Resolution happens at query time.
    pub signal_idx: u32,
    pub stmt_span: Span,
    pub branch: Option<BranchChoice>,
    /// Value assigned to `signal_idx` at this event — i.e. the "to" side of
    /// a transition. The "from" value for a transition at `t_fs` is the
    /// `value` of the preceding `DriverEvent` for the same `signal_idx`
    /// (or the signal's default when no earlier event exists). We keep a
    /// truncated `i64` mirror of the simulator's internal representation;
    /// callers mask to the signal's real width when rendering.
    pub value: i64,
}

impl DriverEvent {
    pub const fn new(
        time_fs: u64,
        signal_idx: u32,
        stmt_span: Span,
        branch: Option<BranchChoice>,
        value: i64,
    ) -> Self {
        Self {
            time_fs,
            signal_idx,
            stmt_span,
            branch,
            value,
        }
    }
}

/// Return the contiguous slice of entries with `time_fs == t_fs` using binary search.
///
/// The trace is assumed to be sorted by `time_fs` (the simulator always appends
/// in time order). Returns an empty slice when no entry matches.
pub fn trace_at(trace: &[TraceEntry], t_fs: u64) -> &[TraceEntry] {
    let lo = trace.partition_point(|e| e.time_fs < t_fs);
    let hi = trace.partition_point(|e| e.time_fs <= t_fs);
    &trace[lo..hi]
}

/// Return the last entry whose `time_fs <= t_fs`.
///
/// Useful when the editor wants the "most recent" statement for a given
/// waveform cursor time (e.g. when hovering between fires of `always`).
pub fn trace_latest_at_or_before(trace: &[TraceEntry], t_fs: u64) -> Option<&TraceEntry> {
    let idx = trace.partition_point(|e| e.time_fs <= t_fs);
    if idx == 0 {
        None
    } else {
        trace.get(idx - 1)
    }
}

/// Find the most recent driver event for `signal_idx` at `time <= t_fs`.
///
/// Linear scan backwards from the end — the expected number of driver events
/// is O(signals × sim ticks), and queries are rare (one per "Jump to Driver"
/// click), so an index is not worth the complexity.
pub fn driver_latest_at_or_before(
    events: &[DriverEvent],
    signal_idx: u32,
    t_fs: u64,
) -> Option<&DriverEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.signal_idx == signal_idx && e.time_fs <= t_fs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_map::Span;

    fn entry(t: u64, file: u16, start: u32, end: u32) -> TraceEntry {
        TraceEntry::new(t, Span::new(file, start, end))
    }

    #[test]
    fn trace_at_returns_contiguous_range() {
        let entries = vec![
            entry(0, 0, 1, 2),
            entry(10, 0, 3, 4),
            entry(10, 0, 5, 6),
            entry(10, 0, 7, 8),
            entry(20, 0, 9, 10),
        ];
        let at_10 = trace_at(&entries, 10);
        assert_eq!(at_10.len(), 3);
        assert!(trace_at(&entries, 5).is_empty());
        assert_eq!(trace_at(&entries, 0).len(), 1);
    }

    #[test]
    fn latest_at_or_before_handles_empty_and_gaps() {
        let entries = vec![entry(5, 0, 1, 2), entry(10, 0, 3, 4)];
        assert_eq!(trace_latest_at_or_before(&entries, 0), None);
        assert_eq!(trace_latest_at_or_before(&entries, 5), Some(&entries[0]));
        assert_eq!(trace_latest_at_or_before(&entries, 7), Some(&entries[0]));
        assert_eq!(trace_latest_at_or_before(&entries, 100), Some(&entries[1]));
    }
}
