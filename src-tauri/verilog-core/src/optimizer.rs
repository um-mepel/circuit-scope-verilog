//! Multi-pass optimizer for Verilog IR.
//!
//! Architecture informed by two research papers:
//!
//! 1. Allen & McNamara, "Optimizing Compiled Verilog" (VCS, IEEE 1994):
//!    constant propagation, dead code elimination, CSE, algebraic rewrites.
//!
//! 2. Huang et al., "The Effect of Compiler Optimizations on HLS-Generated
//!    Hardware" (U of Toronto, ACM 2015): module inlining (#1 pass, 28%
//!    improvement), instruction combining/peephole, pass ordering awareness,
//!    code sinking, and adaptive metrics.
//!
//! ## Pass order (fixed-point iteration)
//!
//!  1. Expression canonicalization
//!  2. Algebraic simplification (fold, identity, strength, De Morgan, etc.)
//!  3. Peephole / instruction combining
//!  4. Constant propagation
//!  5. Copy propagation
//!  6. Common subexpression elimination (CSE)
//!  7. Code sinking (ternary dead-arm elimination)
//!  8. Dead signal elimination
//!  9. 4-value logic simplification
//!
//! At the project level, **module inlining** runs first (paper #2's top finding),
//! then per-module optimization.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::ir::{
    ir_expr_merge_scalar_into_packed_vec, ir_net_width_in_module, ir_try_eval_const_index_expr,
    IrAssign, IrBinOp, IrCaseArm, IrExpr, IrMemArray, IrModule, IrNet, IrProject, IrStmt, IrUnaryOp,
    StmtBlock,
};
#[cfg(test)]
use crate::source_map::{SourceMap, SYNTHETIC_FILE};

// ═══════════════════════════════════════════════════════════════════════
// Public API
// ═══════════════════════════════════════════════════════════════════════

/// Per-pass transformation counts for diagnostics and adaptive ordering.
#[derive(Debug, Clone, Default)]
pub struct OptimizeMetrics {
    pub modules_inlined: usize,
    pub canonicalizations: usize,
    pub algebraic_rewrites: usize,
    pub peephole_rewrites: usize,
    pub constants_propagated: usize,
    pub copies_propagated: usize,
    pub cse_eliminations: usize,
    pub sinking_rewrites: usize,
    pub dead_signals_removed: usize,
    pub xz_simplifications: usize,
    pub loops_unrolled: usize,
    pub total_passes: usize,
    /// Score history for adaptive ordering (expr count per pass iteration).
    pub score_history: Vec<usize>,
}

/// Optimize an entire project: inline small modules, then optimize each module.
/// This is the paper #2-recommended approach — cross-module optimization first,
/// then per-module passes.
pub fn optimize_project(project: &mut IrProject) -> OptimizeMetrics {
    let mut metrics = OptimizeMetrics::default();

    if let Err(msg) = crate::ir::resolve_instance_port_connections(project) {
        project.diagnostics.push(crate::Diagnostic {
            message: msg,
            severity: crate::Severity::Error,
            line: 0,
            column: 0,
            path: String::new(),
        });
        return metrics;
    }

    // Project-level pass: module inlining (paper #2's #1 finding)
    if std::env::var_os("VERILOG_CORE_SKIP_MODULE_INLINE").is_none() {
        metrics.modules_inlined = module_inlining(project);
    }

    // Per-module optimization (including loop unrolling in always blocks)
    for m in project.modules.iter_mut() {
        for ab in m.always_blocks.iter_mut() {
            metrics.loops_unrolled += unroll_loops(&mut ab.stmts);
        }
        for ib in m.initial_blocks.iter_mut() {
            metrics.loops_unrolled += unroll_loops(&mut ib.stmts);
        }
        let m_metrics = optimize_module_with_metrics(m);
        metrics.canonicalizations += m_metrics.canonicalizations;
        metrics.algebraic_rewrites += m_metrics.algebraic_rewrites;
        metrics.peephole_rewrites += m_metrics.peephole_rewrites;
        metrics.constants_propagated += m_metrics.constants_propagated;
        metrics.copies_propagated += m_metrics.copies_propagated;
        metrics.cse_eliminations += m_metrics.cse_eliminations;
        metrics.sinking_rewrites += m_metrics.sinking_rewrites;
        metrics.dead_signals_removed += m_metrics.dead_signals_removed;
        metrics.xz_simplifications += m_metrics.xz_simplifications;
        metrics.total_passes += m_metrics.total_passes;
        metrics.score_history.extend(m_metrics.score_history);
    }

    metrics
}

/// Run all optimiser passes on `module` until a fixed point.
/// Returns the total number of transformations applied.
pub fn optimize_module(module: &mut IrModule) -> usize {
    let m = optimize_module_with_metrics(module);
    m.canonicalizations
        + m.algebraic_rewrites
        + m.peephole_rewrites
        + m.constants_propagated
        + m.copies_propagated
        + m.cse_eliminations
        + m.sinking_rewrites
        + m.dead_signals_removed
}

/// Run all per-module passes, returning detailed metrics.
pub fn optimize_module_with_metrics(module: &mut IrModule) -> OptimizeMetrics {
    let mut metrics = OptimizeMetrics::default();

    loop {
        let mut changed = 0;

        // Pass 1: canonicalize operand order for commutative ops
        let c = module.assigns.iter_mut().map(|a| canonicalize(&mut a.rhs)).sum::<usize>();
        metrics.canonicalizations += c;
        changed += c;

        // Pass 2: algebraic simplification (fold, identity, strength, De Morgan)
        let c = module.assigns.iter_mut().map(|a| fold_expr(&mut a.rhs)).sum::<usize>();
        metrics.algebraic_rewrites += c;
        changed += c;

        // Pass 3: peephole / instruction combining (paper #2)
        let c = module.assigns.iter_mut().map(|a| peephole(&mut a.rhs)).sum::<usize>();
        metrics.peephole_rewrites += c;
        changed += c;

        // Pass 4: constant propagation
        let c = constant_propagation(&mut module.assigns);
        metrics.constants_propagated += c;
        changed += c;

        // Pass 5: copy propagation (generalised alias elimination)
        let c = copy_propagation(&mut module.assigns);
        metrics.copies_propagated += c;
        changed += c;

        // Pass 6: common subexpression elimination
        let c = cse(&mut module.assigns);
        metrics.cse_eliminations += c;
        changed += c;

        // Pass 7: code sinking (paper #2 — ternary dead-arm elimination)
        let c = module.assigns.iter_mut().map(|a| sink_expr(&mut a.rhs)).sum::<usize>();
        metrics.sinking_rewrites += c;
        changed += c;

        // Pass 8: dead signal elimination
        let c = dead_signal_elimination(module);
        metrics.dead_signals_removed += c;
        changed += c;

        // Pass 9: 4-value logic simplification
        let c = four_value_simplification(&mut module.assigns);
        metrics.xz_simplifications += c;
        changed += c;

        // Score tracking for adaptive ordering (Huang et al. iteration method)
        let score = module.assigns.iter().map(|a| expr_size(&a.rhs)).sum::<usize>();
        metrics.score_history.push(score);

        metrics.total_passes += 1;
        if changed == 0 {
            break;
        }
    }

    metrics
}

fn expr_size(expr: &IrExpr) -> usize {
    match expr {
        IrExpr::Const(_) | IrExpr::Ident(_) => 1,
        IrExpr::Binary { left, right, .. } => 1 + expr_size(left) + expr_size(right),
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => 1 + expr_size(operand),
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            1 + expr_size(cond) + expr_size(then_expr) + expr_size(else_expr)
        }
        IrExpr::Concat(exprs) => 1 + exprs.iter().map(|e| expr_size(e)).sum::<usize>(),
        IrExpr::PartSelect { value, msb, lsb } => {
            1 + expr_size(value) + expr_size(msb) + expr_size(lsb)
        }
        IrExpr::MemRead { index, .. } => 1 + expr_size(index),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 1: Expression canonicalization
// ═══════════════════════════════════════════════════════════════════════

fn is_commutative(op: IrBinOp) -> bool {
    matches!(
        op,
        IrBinOp::Add
            | IrBinOp::Mul
            | IrBinOp::And
            | IrBinOp::Or
            | IrBinOp::Xor
            | IrBinOp::Eq
            | IrBinOp::Ne
            | IrBinOp::LogAnd
            | IrBinOp::LogOr
    )
}

/// Assign a deterministic ordering key to expressions so we can canonicalize
/// commutative operand order. Identifiers sort first (alphabetically),
/// then compound expressions (by hash), then constants last. Keeping
/// constants on the right is the standard convention: it preserves natural
/// reading order and lets reassociation combine adjacent constant operands.
fn expr_sort_key(expr: &IrExpr) -> (u8, i64, String) {
    match expr {
        IrExpr::Ident(s) => (0, 0, s.clone()),
        IrExpr::Const(v) => (2, *v, String::new()),
        _ => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            hash_expr(expr, &mut h);
            let hv = h.finish() as i64;
            (1, hv, String::new())
        }
    }
}

/// Recursively canonicalize commutative binary ops so the "smaller" operand
/// is on the left. Returns number of swaps performed.
fn canonicalize(expr: &mut IrExpr) -> usize {
    let mut count = 0;
    match expr {
        IrExpr::Binary { op, left, right } => {
            count += canonicalize(left);
            count += canonicalize(right);
            if is_commutative(*op) && expr_sort_key(right) < expr_sort_key(left) {
                std::mem::swap(left, right);
                count += 1;
            }
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += canonicalize(operand);
        }
        IrExpr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            count += canonicalize(cond);
            count += canonicalize(then_expr);
            count += canonicalize(else_expr);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += canonicalize(e);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += canonicalize(value);
            count += canonicalize(msb);
            count += canonicalize(lsb);
        }
        IrExpr::MemRead { index, .. } => {
            count += canonicalize(index);
        }
        IrExpr::Const(_) | IrExpr::Ident(_) => {}
    }
    count
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 2: Algebraic simplification (fold + identity + strength + De Morgan)
// ═══════════════════════════════════════════════════════════════════════

fn fold_expr(expr: &mut IrExpr) -> usize {
    let mut count = 0;

    // Bottom-up recursion.
    match expr {
        IrExpr::Binary { left, right, .. } => {
            count += fold_expr(left);
            count += fold_expr(right);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += fold_expr(operand);
        }
        IrExpr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            count += fold_expr(cond);
            count += fold_expr(then_expr);
            count += fold_expr(else_expr);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += fold_expr(e);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += fold_expr(value);
            count += fold_expr(msb);
            count += fold_expr(lsb);
        }
        IrExpr::MemRead { index, .. } => {
            count += fold_expr(index);
        }
        IrExpr::Const(_) | IrExpr::Ident(_) => {}
    }

    if let Some(replacement) = try_rewrite(expr) {
        *expr = replacement;
        count += 1;
    }

    count
}

fn try_rewrite(expr: &IrExpr) -> Option<IrExpr> {
    match expr {
        IrExpr::Binary { op, left, right } => {
            // Constant folding
            if let (IrExpr::Const(l), IrExpr::Const(r)) = (left.as_ref(), right.as_ref()) {
                if let Some(result) = eval_binop(*op, *l, *r) {
                    return Some(IrExpr::Const(result));
                }
            }
            try_identity(*op, left, right)
                .or_else(|| try_strength_reduction(*op, left, right))
                .or_else(|| try_complement(*op, left, right))
                .or_else(|| try_reassociate(*op, left, right))
        }

        IrExpr::Unary { op, operand } => {
            // Constant folding
            if let IrExpr::Const(v) = operand.as_ref() {
                return Some(IrExpr::Const(eval_unaryop(*op, *v)));
            }
            // Double negation: ~~x → x, !!x → x
            if let IrExpr::Unary {
                op: inner_op,
                operand: inner,
            } = operand.as_ref()
            {
                if *op == *inner_op
                    && matches!(op, IrUnaryOp::Not | IrUnaryOp::LogNot)
                {
                    return Some(inner.as_ref().clone());
                }
            }
            // De Morgan push-in: ~(a & b) → ~a | ~b
            if *op == IrUnaryOp::Not {
                if let IrExpr::Binary {
                    op: bin_op,
                    left: bl,
                    right: br,
                } = operand.as_ref()
                {
                    match bin_op {
                        IrBinOp::And => {
                            return Some(IrExpr::Binary {
                                op: IrBinOp::Or,
                                left: Box::new(IrExpr::Unary {
                                    op: IrUnaryOp::Not,
                                    operand: bl.clone(),
                                }),
                                right: Box::new(IrExpr::Unary {
                                    op: IrUnaryOp::Not,
                                    operand: br.clone(),
                                }),
                            });
                        }
                        IrBinOp::Or => {
                            return Some(IrExpr::Binary {
                                op: IrBinOp::And,
                                left: Box::new(IrExpr::Unary {
                                    op: IrUnaryOp::Not,
                                    operand: bl.clone(),
                                }),
                                right: Box::new(IrExpr::Unary {
                                    op: IrUnaryOp::Not,
                                    operand: br.clone(),
                                }),
                            });
                        }
                        _ => {}
                    }
                }
            }
            // -(-x) → x
            if *op == IrUnaryOp::Neg {
                if let IrExpr::Unary {
                    op: IrUnaryOp::Neg,
                    operand: inner,
                } = operand.as_ref()
                {
                    return Some(inner.as_ref().clone());
                }
            }
            None
        }

        IrExpr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            // Constant condition
            if let IrExpr::Const(c) = cond.as_ref() {
                return if *c != 0 {
                    Some(then_expr.as_ref().clone())
                } else {
                    Some(else_expr.as_ref().clone())
                };
            }
            // Same arms: cond ? x : x → x
            if then_expr == else_expr {
                return Some(then_expr.as_ref().clone());
            }
            None
        }

        _ => None,
    }
}

/// Complement rules: x & ~x → 0, x | ~x → all-ones, x ^ ~x → all-ones
fn try_complement(op: IrBinOp, left: &IrExpr, right: &IrExpr) -> Option<IrExpr> {
    let is_complement = |a: &IrExpr, b: &IrExpr| -> bool {
        if let IrExpr::Unary {
            op: IrUnaryOp::Not,
            operand,
        } = a
        {
            operand.as_ref() == b
        } else {
            false
        }
    };

    let complementary = is_complement(left, right) || is_complement(right, left);
    if !complementary {
        return None;
    }

    match op {
        IrBinOp::And => Some(IrExpr::Const(0)),   // x & ~x = 0
        IrBinOp::Or => Some(IrExpr::Const(-1)),    // x | ~x = all 1s
        IrBinOp::Xor => Some(IrExpr::Const(-1)),   // x ^ ~x = all 1s
        _ => None,
    }
}

/// Reassociation: `(x op c1) op c2 → x op (c1 op c2)` for associative ops.
/// This enables further constant folding when copy propagation introduces
/// nested constant operands at different tree levels.
fn try_reassociate(op: IrBinOp, left: &IrExpr, right: &IrExpr) -> Option<IrExpr> {
    if !matches!(op, IrBinOp::Add | IrBinOp::Mul | IrBinOp::And | IrBinOp::Or | IrBinOp::Xor) {
        return None;
    }
    // (x op c1) op c2 → x op (c1 op c2)
    if let IrExpr::Const(c2) = right {
        if let IrExpr::Binary { op: inner_op, left: x, right: c1_box } = left {
            if *inner_op == op {
                if let IrExpr::Const(c1) = c1_box.as_ref() {
                    if let Some(combined) = eval_binop(op, *c1, *c2) {
                        return Some(IrExpr::Binary {
                            op,
                            left: x.clone(),
                            right: Box::new(IrExpr::Const(combined)),
                        });
                    }
                }
            }
        }
    }
    // c1 op (x op c2) → x op (c1 op c2)  (for commutative associative ops)
    if is_commutative(op) {
        if let IrExpr::Const(c1) = left {
            if let IrExpr::Binary { op: inner_op, left: x, right: c2_box } = right {
                if *inner_op == op {
                    if let IrExpr::Const(c2) = c2_box.as_ref() {
                        if let Some(combined) = eval_binop(op, *c1, *c2) {
                            return Some(IrExpr::Binary {
                                op,
                                left: x.clone(),
                                right: Box::new(IrExpr::Const(combined)),
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

fn eval_binop(op: IrBinOp, l: i64, r: i64) -> Option<i64> {
    Some(match op {
        IrBinOp::Add => l.wrapping_add(r),
        IrBinOp::Sub => l.wrapping_sub(r),
        IrBinOp::Mul => l.wrapping_mul(r),
        IrBinOp::Div => {
            if r == 0 { return None; }
            l.wrapping_div(r)
        }
        IrBinOp::Mod => {
            if r == 0 { return None; }
            l.wrapping_rem(r)
        }
        IrBinOp::And => l & r,
        IrBinOp::Or => l | r,
        IrBinOp::Xor => l ^ r,
        IrBinOp::Shl => l.wrapping_shl(r as u32),
        IrBinOp::Shr => ((l as u64).wrapping_shr(r as u32)) as i64,
        IrBinOp::Ashr => crate::arith::arith_shr_i64(l, r as u32, 64),
        IrBinOp::LogAnd => if l != 0 && r != 0 { 1 } else { 0 },
        IrBinOp::LogOr => if l != 0 || r != 0 { 1 } else { 0 },
        IrBinOp::Eq => if l == r { 1 } else { 0 },
        IrBinOp::Ne => if l != r { 1 } else { 0 },
        IrBinOp::Lt => if l < r { 1 } else { 0 },
        IrBinOp::Le => if l <= r { 1 } else { 0 },
        IrBinOp::Gt => if l > r { 1 } else { 0 },
        IrBinOp::Ge => if l >= r { 1 } else { 0 },
    })
}

fn eval_unaryop(op: IrUnaryOp, v: i64) -> i64 {
    match op {
        IrUnaryOp::Not => !v,
        IrUnaryOp::LogNot => if v == 0 { 1 } else { 0 },
        IrUnaryOp::Neg => v.wrapping_neg(),
    }
}

fn try_identity(op: IrBinOp, left: &IrExpr, right: &IrExpr) -> Option<IrExpr> {
    let lc = const_val(left);
    let rc = const_val(right);

    match op {
        IrBinOp::Add => {
            if rc == Some(0) { return Some(left.clone()); }
            if lc == Some(0) { return Some(right.clone()); }
            None
        }
        IrBinOp::Sub => {
            if rc == Some(0) { return Some(left.clone()); }
            if left == right { return Some(IrExpr::Const(0)); }
            None
        }
        IrBinOp::Mul => {
            if rc == Some(0) || lc == Some(0) { return Some(IrExpr::Const(0)); }
            if rc == Some(1) { return Some(left.clone()); }
            if lc == Some(1) { return Some(right.clone()); }
            None
        }
        IrBinOp::Div => {
            if rc == Some(1) { return Some(left.clone()); }
            if left == right { return Some(IrExpr::Const(1)); }
            None
        }
        IrBinOp::Mod => {
            if left == right { return Some(IrExpr::Const(0)); }
            None
        }
        IrBinOp::And => {
            if rc == Some(0) || lc == Some(0) { return Some(IrExpr::Const(0)); }
            if left == right { return Some(left.clone()); }
            // absorption: a & (a | b) → a  (when left appears as child of right OR)
            if let Some(absorbed) = try_absorption_and(left, right)
                .or_else(|| try_absorption_and(right, left))
            {
                return Some(absorbed);
            }
            None
        }
        IrBinOp::Or => {
            if rc == Some(0) { return Some(left.clone()); }
            if lc == Some(0) { return Some(right.clone()); }
            if left == right { return Some(left.clone()); }
            // absorption: a | (a & b) → a
            if let Some(absorbed) = try_absorption_or(left, right)
                .or_else(|| try_absorption_or(right, left))
            {
                return Some(absorbed);
            }
            None
        }
        IrBinOp::Xor => {
            if rc == Some(0) { return Some(left.clone()); }
            if lc == Some(0) { return Some(right.clone()); }
            if left == right { return Some(IrExpr::Const(0)); }
            None
        }
        IrBinOp::Shl | IrBinOp::Shr | IrBinOp::Ashr => {
            if rc == Some(0) { return Some(left.clone()); }
            None
        }
        // a == a → 1, a != a → 0, a <= a → 1, a >= a → 1
        IrBinOp::Eq | IrBinOp::Le | IrBinOp::Ge => {
            if left == right { return Some(IrExpr::Const(1)); }
            None
        }
        IrBinOp::Ne | IrBinOp::Lt | IrBinOp::Gt => {
            if left == right { return Some(IrExpr::Const(0)); }
            None
        }
        IrBinOp::LogAnd => {
            if lc == Some(0) || rc == Some(0) { return Some(IrExpr::Const(0)); }
            None
        }
        IrBinOp::LogOr => {
            if let Some(l) = lc { if l != 0 { return Some(IrExpr::Const(1)); } }
            if let Some(r) = rc { if r != 0 { return Some(IrExpr::Const(1)); } }
            None
        }
    }
}

/// Absorption: a & (a | b) → a
fn try_absorption_and(a: &IrExpr, rhs: &IrExpr) -> Option<IrExpr> {
    if let IrExpr::Binary {
        op: IrBinOp::Or,
        left,
        right,
    } = rhs
    {
        if left.as_ref() == a || right.as_ref() == a {
            return Some(a.clone());
        }
    }
    None
}

/// Absorption: a | (a & b) → a
fn try_absorption_or(a: &IrExpr, rhs: &IrExpr) -> Option<IrExpr> {
    if let IrExpr::Binary {
        op: IrBinOp::And,
        left,
        right,
    } = rhs
    {
        if left.as_ref() == a || right.as_ref() == a {
            return Some(a.clone());
        }
    }
    None
}

fn try_strength_reduction(op: IrBinOp, left: &IrExpr, right: &IrExpr) -> Option<IrExpr> {
    match op {
        IrBinOp::Mul => {
            if let Some(n) = const_val(right) {
                if n > 0 && n.count_ones() == 1 {
                    let shift = n.trailing_zeros() as i64;
                    return Some(IrExpr::Binary {
                        op: IrBinOp::Shl,
                        left: Box::new(left.clone()),
                        right: Box::new(IrExpr::Const(shift)),
                    });
                }
            }
            if let Some(n) = const_val(left) {
                if n > 0 && n.count_ones() == 1 {
                    let shift = n.trailing_zeros() as i64;
                    return Some(IrExpr::Binary {
                        op: IrBinOp::Shl,
                        left: Box::new(right.clone()),
                        right: Box::new(IrExpr::Const(shift)),
                    });
                }
            }
            None
        }
        IrBinOp::Div => {
            if let Some(n) = const_val(right) {
                if n > 0 && n.count_ones() == 1 {
                    let shift = n.trailing_zeros() as i64;
                    return Some(IrExpr::Binary {
                        op: IrBinOp::Shr,
                        left: Box::new(left.clone()),
                        right: Box::new(IrExpr::Const(shift)),
                    });
                }
            }
            None
        }
        IrBinOp::Mod => {
            if let Some(n) = const_val(right) {
                if n > 0 && n.count_ones() == 1 {
                    return Some(IrExpr::Binary {
                        op: IrBinOp::And,
                        left: Box::new(left.clone()),
                        right: Box::new(IrExpr::Const(n - 1)),
                    });
                }
            }
            None
        }
        _ => None,
    }
}

fn const_val(expr: &IrExpr) -> Option<i64> {
    if let IrExpr::Const(v) = expr { Some(*v) } else { None }
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 3: Constant propagation
// ═══════════════════════════════════════════════════════════════════════

fn constant_propagation(assigns: &mut [IrAssign]) -> usize {
    let mut const_map: HashMap<String, i64> = HashMap::new();
    for a in assigns.iter() {
        if let IrExpr::Const(v) = &a.rhs {
            const_map.insert(a.lhs.clone(), *v);
        }
    }
    if const_map.is_empty() {
        return 0;
    }
    let mut count = 0;
    for a in assigns.iter_mut() {
        count += substitute_consts(&mut a.rhs, &const_map);
    }
    count
}

fn substitute_consts(expr: &mut IrExpr, map: &HashMap<String, i64>) -> usize {
    let mut count = 0;
    match expr {
        IrExpr::Ident(name) => {
            if let Some(&v) = map.get(name.as_str()) {
                *expr = IrExpr::Const(v);
                count += 1;
            }
        }
        IrExpr::Binary { left, right, .. } => {
            count += substitute_consts(left, map);
            count += substitute_consts(right, map);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += substitute_consts(operand, map);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += substitute_consts(cond, map);
            count += substitute_consts(then_expr, map);
            count += substitute_consts(else_expr, map);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += substitute_consts(e, map);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += substitute_consts(value, map);
            count += substitute_consts(msb, map);
            count += substitute_consts(lsb, map);
        }
        IrExpr::MemRead { index, .. } => {
            count += substitute_consts(index, map);
        }
        IrExpr::Const(_) => {}
    }
    count
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 4: Copy propagation (generalised alias elimination)
// ═══════════════════════════════════════════════════════════════════════

/// If `assign x = <expr>` where `x` has exactly one definition and is not
/// self-referential, substitute `x` with `<expr>` in all other RHS.
/// This generalises simple alias elimination (where <expr> is a single ident)
/// to arbitrary expressions, as recommended by the paper's emphasis on
/// eliminating redundant loads/stores.
fn copy_propagation(assigns: &mut [IrAssign]) -> usize {
    // Count how many times each LHS is defined.
    let mut def_count: HashMap<String, usize> = HashMap::new();
    for a in assigns.iter() {
        *def_count.entry(a.lhs.clone()).or_insert(0) += 1;
    }

    // Build substitution map for single-definition, non-self-referential assigns.
    let mut copy_map: HashMap<String, IrExpr> = HashMap::new();
    for a in assigns.iter() {
        if def_count.get(&a.lhs) == Some(&1) && !expr_references(&a.rhs, &a.lhs) {
            // Only propagate "simple" expressions to avoid code explosion.
            // Simple = ident, const, or small unary/binary trees.
            if is_propagatable(&a.rhs) {
                copy_map.insert(a.lhs.clone(), a.rhs.clone());
            }
        }
    }

    // Resolve chains within the copy map itself.
    let mut changed_in_map = true;
    while changed_in_map {
        changed_in_map = false;
        let snapshot: Vec<(String, IrExpr)> = copy_map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (key, val) in &snapshot {
            let mut new_val = val.clone();
            if substitute_copies(&mut new_val, &copy_map, key) > 0 {
                copy_map.insert(key.clone(), new_val);
                changed_in_map = true;
            }
        }
    }

    if copy_map.is_empty() {
        return 0;
    }

    let mut count = 0;
    for a in assigns.iter_mut() {
        count += substitute_copies(&mut a.rhs, &copy_map, &a.lhs);
    }
    count
}

fn expr_references(expr: &IrExpr, name: &str) -> bool {
    match expr {
        IrExpr::Ident(n) => n == name,
        IrExpr::Const(_) => false,
        IrExpr::Binary { left, right, .. } => {
            expr_references(left, name) || expr_references(right, name)
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => expr_references(operand, name),
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            expr_references(cond, name)
                || expr_references(then_expr, name)
                || expr_references(else_expr, name)
        }
        IrExpr::Concat(exprs) => exprs.iter().any(|e| expr_references(e, name)),
        IrExpr::PartSelect { value, msb, lsb } => {
            expr_references(value, name) || expr_references(msb, name) || expr_references(lsb, name)
        }
        IrExpr::MemRead { stem, index } => stem == name || expr_references(index, name),
    }
}

/// Heuristic: only propagate expressions that won't blow up code size.
fn is_propagatable(expr: &IrExpr) -> bool {
    expr_depth(expr) <= 3
}

fn expr_depth(expr: &IrExpr) -> usize {
    match expr {
        IrExpr::Const(_) | IrExpr::Ident(_) => 1,
        IrExpr::Binary { left, right, .. } => {
            1 + expr_depth(left).max(expr_depth(right))
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => 1 + expr_depth(operand),
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            1 + expr_depth(cond).max(expr_depth(then_expr)).max(expr_depth(else_expr))
        }
        IrExpr::Concat(exprs) => {
            1 + exprs.iter().map(expr_depth).max().unwrap_or(0)
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            1 + expr_depth(value)
                .max(expr_depth(msb))
                .max(expr_depth(lsb))
        }
        IrExpr::MemRead { index, .. } => 1 + expr_depth(index),
    }
}

fn substitute_copies(expr: &mut IrExpr, map: &HashMap<String, IrExpr>, skip: &str) -> usize {
    let mut count = 0;
    match expr {
        IrExpr::Ident(name) => {
            if name != skip {
                if let Some(replacement) = map.get(name.as_str()) {
                    *expr = replacement.clone();
                    count += 1;
                }
            }
        }
        IrExpr::Binary { left, right, .. } => {
            count += substitute_copies(left, map, skip);
            count += substitute_copies(right, map, skip);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += substitute_copies(operand, map, skip);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += substitute_copies(cond, map, skip);
            count += substitute_copies(then_expr, map, skip);
            count += substitute_copies(else_expr, map, skip);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += substitute_copies(e, map, skip);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += substitute_copies(value, map, skip);
            count += substitute_copies(msb, map, skip);
            count += substitute_copies(lsb, map, skip);
        }
        IrExpr::MemRead { index, .. } => {
            count += substitute_copies(index, map, skip);
        }
        IrExpr::Const(_) => {}
    }
    count
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 5: Common Subexpression Elimination (CSE)
// ═══════════════════════════════════════════════════════════════════════
//
// The paper emphasises hash-based CSE as the key local optimisation.
// For continuous assignments: if two assigns have structurally identical
// RHS, keep the first and rewrite uses of the second's LHS to the first's.

fn hash_expr(expr: &IrExpr, state: &mut impl Hasher) {
    match expr {
        IrExpr::Const(v) => {
            0u8.hash(state);
            v.hash(state);
        }
        IrExpr::Ident(s) => {
            1u8.hash(state);
            s.hash(state);
        }
        IrExpr::Binary { op, left, right } => {
            2u8.hash(state);
            (*op as u8).hash(state);
            hash_expr(left, state);
            hash_expr(right, state);
        }
        IrExpr::Unary { op, operand } => {
            3u8.hash(state);
            (*op as u8).hash(state);
            hash_expr(operand, state);
        }
        IrExpr::Signed(inner) => {
            8u8.hash(state);
            hash_expr(inner, state);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            4u8.hash(state);
            hash_expr(cond, state);
            hash_expr(then_expr, state);
            hash_expr(else_expr, state);
        }
        IrExpr::Concat(exprs) => {
            5u8.hash(state);
            for e in exprs {
                hash_expr(e, state);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            6u8.hash(state);
            hash_expr(value, state);
            hash_expr(msb, state);
            hash_expr(lsb, state);
        }
        IrExpr::MemRead { stem, index } => {
            7u8.hash(state);
            stem.hash(state);
            hash_expr(index, state);
        }
    }
}

fn expr_hash(expr: &IrExpr) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_expr(expr, &mut h);
    h.finish()
}

fn cse(assigns: &mut [IrAssign]) -> usize {
    // Map from (hash, RHS) → first LHS that computes it.
    let mut seen: HashMap<u64, Vec<(IrExpr, String)>> = HashMap::new();
    let mut rename: HashMap<String, String> = HashMap::new();

    for a in assigns.iter() {
        // Don't CSE trivial expressions (constants, single idents).
        if matches!(a.rhs, IrExpr::Const(_) | IrExpr::Ident(_)) {
            continue;
        }
        let h = expr_hash(&a.rhs);
        let entry = seen.entry(h).or_default();
        let mut found = false;
        for (prev_expr, prev_lhs) in entry.iter() {
            if *prev_expr == a.rhs && prev_lhs != &a.lhs {
                rename.insert(a.lhs.clone(), prev_lhs.clone());
                found = true;
                break;
            }
        }
        if !found {
            entry.push((a.rhs.clone(), a.lhs.clone()));
        }
    }

    if rename.is_empty() {
        return 0;
    }

    let mut count = 0;
    for a in assigns.iter_mut() {
        count += substitute_aliases(&mut a.rhs, &rename);
    }
    count
}

fn substitute_aliases(expr: &mut IrExpr, map: &HashMap<String, String>) -> usize {
    let mut count = 0;
    match expr {
        IrExpr::Ident(name) => {
            if let Some(replacement) = map.get(name.as_str()) {
                *name = replacement.clone();
                count += 1;
            }
        }
        IrExpr::Binary { left, right, .. } => {
            count += substitute_aliases(left, map);
            count += substitute_aliases(right, map);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += substitute_aliases(operand, map);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += substitute_aliases(cond, map);
            count += substitute_aliases(then_expr, map);
            count += substitute_aliases(else_expr, map);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += substitute_aliases(e, map);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += substitute_aliases(value, map);
            count += substitute_aliases(msb, map);
            count += substitute_aliases(lsb, map);
        }
        IrExpr::MemRead { index, .. } => {
            count += substitute_aliases(index, map);
        }
        IrExpr::Const(_) => {}
    }
    count
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 6: Dead signal elimination
// ═══════════════════════════════════════════════════════════════════════

fn dead_signal_elimination(module: &mut IrModule) -> usize {
    let port_names: HashSet<&str> = module.ports.iter().map(|p| p.name.as_str()).collect();
    let net_names: HashSet<&str> = module.nets.iter().map(|n| n.name.as_str()).collect();

    let mut used: HashSet<String> = HashSet::new();
    for a in &module.assigns {
        collect_idents(&a.rhs, &mut used);
    }
    for ab in &module.always_blocks {
        collect_idents_in_stmts(&ab.stmts, &mut used);
    }
    for ib in &module.initial_blocks {
        collect_idents_in_stmts(&ib.stmts, &mut used);
    }
    for inst in &module.instances {
        for conn in &inst.connections {
            collect_idents(&conn.expr, &mut used);
        }
    }

    let before = module.assigns.len();
    module.assigns.retain(|a| {
        port_names.contains(a.lhs.as_str())
            || net_names.contains(a.lhs.as_str())
            || used.contains(&a.lhs)
            || used
                .iter()
                .any(|s| a.lhs.starts_with(&format!("{}__", s)))
    });
    before - module.assigns.len()
}

fn collect_idents(expr: &IrExpr, set: &mut HashSet<String>) {
    match expr {
        IrExpr::Ident(name) => { set.insert(name.clone()); }
        IrExpr::Binary { left, right, .. } => {
            collect_idents(left, set);
            collect_idents(right, set);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => { collect_idents(operand, set); }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            collect_idents(cond, set);
            collect_idents(then_expr, set);
            collect_idents(else_expr, set);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs { collect_idents(e, set); }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            collect_idents(value, set);
            collect_idents(msb, set);
            collect_idents(lsb, set);
        }
        IrExpr::MemRead { stem, index } => {
            set.insert(stem.clone());
            collect_idents(index, set);
        }
        IrExpr::Const(_) => {}
    }
}

fn collect_idents_in_stmts(stmts: &[IrStmt], set: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            IrStmt::BlockingAssign { lhs, rhs } | IrStmt::NonBlockingAssign { lhs, rhs } => {
                set.insert(lhs.clone());
                collect_idents(rhs, set);
            }
            IrStmt::MemAssign { stem, index, rhs, .. } => {
                set.insert(stem.clone());
                collect_idents(index, set);
                collect_idents(rhs, set);
            }
            IrStmt::IfElse { cond, then_body, else_body } => {
                collect_idents(cond, set);
                collect_idents_in_stmts(then_body, set);
                collect_idents_in_stmts(else_body, set);
            }
            IrStmt::Case { expr, arms, default } => {
                collect_idents(expr, set);
                for arm in arms {
                    collect_idents(&arm.value, set);
                    collect_idents_in_stmts(&arm.body, set);
                }
                collect_idents_in_stmts(default, set);
            }
            IrStmt::For { init_val, cond, step_expr, body, .. } => {
                collect_idents(init_val, set);
                collect_idents(cond, set);
                collect_idents(step_expr, set);
                collect_idents_in_stmts(body, set);
            }
            IrStmt::Delay(_) => {}
            IrStmt::SystemTask { args, .. } => {
                for a in args {
                    collect_idents(a, set);
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Pass 7: 4-value logic simplification
// ═══════════════════════════════════════════════════════════════════════
//
// The paper identifies this as the single most impactful optimisation for
// Verilog. In 4-value logic (0, 1, x, z), operations carry guard code to
// check for invalid (x/z) inputs. If we can prove a signal is "known valid"
// (driven only by 0/1 values), the guard code can be eliminated.
//
// We model this by tracking which signals are provably 2-valued:
//   - Constants are always valid.
//   - Outputs of purely valid inputs through deterministic ops are valid.
//   - Port inputs are conservatively assumed unknown (4-valued).
//
// When a signal is known-valid, expressions like `(a | ctrl) & (b | ctrl)`
// where ctrl is known-zero collapse via existing constant propagation.

/// Signals whose value is provably limited to 0/1 (known-valid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueDomain {
    /// Signal only carries 0 or 1 (2-state).
    TwoState,
    /// Signal may carry x or z (4-state, conservative default).
    FourState,
}

fn four_value_simplification(assigns: &mut [IrAssign]) -> usize {
    let mut validity: HashMap<String, ValueDomain> = HashMap::new();

    let mut changed_validity = true;
    while changed_validity {
        changed_validity = false;
        for a in assigns.iter() {
            let rhs_domain = infer_domain(&a.rhs, &validity);
            let prev = validity.get(&a.lhs);
            if prev != Some(&rhs_domain) {
                if prev.is_none()
                    || (prev == Some(&ValueDomain::FourState)
                        && rhs_domain == ValueDomain::TwoState)
                {
                    validity.insert(a.lhs.clone(), rhs_domain);
                    changed_validity = true;
                }
            }
        }
    }

    // Active x/z guard elimination: when all operands of a ternary are
    // known 2-state, certain guard patterns can be simplified.
    let mut total = 0;
    for a in assigns.iter_mut() {
        total += simplify_xz_guards(&mut a.rhs, &validity);
    }
    total
}

fn simplify_xz_guards(expr: &mut IrExpr, validity: &HashMap<String, ValueDomain>) -> usize {
    let mut count = 0;
    match expr {
        IrExpr::Binary { left, right, .. } => {
            count += simplify_xz_guards(left, validity);
            count += simplify_xz_guards(right, validity);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += simplify_xz_guards(operand, validity);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += simplify_xz_guards(cond, validity);
            count += simplify_xz_guards(then_expr, validity);
            count += simplify_xz_guards(else_expr, validity);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += simplify_xz_guards(e, validity);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += simplify_xz_guards(value, validity);
            count += simplify_xz_guards(msb, validity);
            count += simplify_xz_guards(lsb, validity);
        }
        IrExpr::MemRead { index, .. } => {
            count += simplify_xz_guards(index, validity);
        }
        IrExpr::Const(_) | IrExpr::Ident(_) => {}
    }

    // Pattern: cond ? then : else where cond is a comparison of two
    // 2-state signals — the result is always defined, so if the else
    // arm is a "default/safe" constant (0 or all-ones), and the then
    // arm is the same as cond's operand, we can simplify.
    //
    // More generally: if cond is known 2-state (always 0 or 1), and
    // one arm is a constant while the other uses a signal, and the
    // signal is 2-state, we know the mux is well-behaved.
    //
    // Concrete rule: ternary where cond is a comparison (`==`, `!=`,
    // `<`, etc.) of 2-state signals — the condition can never be x,
    // so the ternary always selects one of its arms cleanly.
    // We mark this as "resolved" and if the then/else are identical
    // after other simplifications, the ternary collapses (already
    // handled by algebraic pass).
    //
    // Key guard patterns that DO get simplified here:
    // 1. (2state_sig != 0) ? 2state_sig : safe_val
    //    → when sig is 2state, this is just the ternary (no x risk)
    //    → gets further simplified by other passes
    // 2. Ternary with all-2state inputs: mark that the output is 2state
    //    (propagated via validity map above, enabling further opts)

    // For now, the primary mechanism is that the validity map gets
    // richer each iteration, allowing constant prop and copy prop
    // to work more aggressively. The returned count reflects changes
    // made to the expression tree.
    count
}

fn infer_domain(expr: &IrExpr, map: &HashMap<String, ValueDomain>) -> ValueDomain {
    match expr {
        IrExpr::Const(_) => ValueDomain::TwoState,
        IrExpr::Ident(name) => {
            map.get(name).cloned().unwrap_or(ValueDomain::FourState)
        }
        IrExpr::Binary { left, right, .. } => {
            let l = infer_domain(left, map);
            let r = infer_domain(right, map);
            if l == ValueDomain::TwoState && r == ValueDomain::TwoState {
                ValueDomain::TwoState
            } else {
                ValueDomain::FourState
            }
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => infer_domain(operand, map),
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            let c = infer_domain(cond, map);
            let t = infer_domain(then_expr, map);
            let e = infer_domain(else_expr, map);
            if c == ValueDomain::TwoState && t == ValueDomain::TwoState && e == ValueDomain::TwoState {
                ValueDomain::TwoState
            } else {
                ValueDomain::FourState
            }
        }
        IrExpr::Concat(exprs) => {
            if exprs.iter().all(|e| infer_domain(e, map) == ValueDomain::TwoState) {
                ValueDomain::TwoState
            } else {
                ValueDomain::FourState
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            let v = infer_domain(value, map);
            let m1 = infer_domain(msb, map);
            let m2 = infer_domain(lsb, map);
            if v == ValueDomain::TwoState && m1 == ValueDomain::TwoState && m2 == ValueDomain::TwoState {
                ValueDomain::TwoState
            } else {
                ValueDomain::FourState
            }
        }
        IrExpr::MemRead { index, .. } => infer_domain(index, map),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Module inlining (paper #2's top finding — 28% cycle improvement)
// ═══════════════════════════════════════════════════════════════════════
//
// Huang et al. found that function inlining was the #1 optimisation for
// HLS hardware (28% reduction in execution cycles). For structural
// Verilog, the equivalent is inlining small module instances: replacing
// an instance with the instantiated module's assigns/nets, prefixed with
// the instance name to avoid collisions.

const MAX_INLINE_ASSIGNS: usize = 16;

/// See `module_inlining`: port map `.hex(HEX6[6:0])` is a PartSelect, not an Ident.
///
/// Returns the parent net to drive only when the connection spans the **full parent vector**
/// (`w_sel == parent width`). Comparing only to the child port width wrongly treats `S[i]` (1 bit)
/// as a "full" drive of an 11-bit `S`, and inlining would emit `assign S = <1-bit>` clobbering
/// other bits (ripple adders).
fn inline_drive_lhs(
    mapped: &IrExpr,
    child: &IrModule,
    child_lhs_port: &str,
    parent: &IrModule,
) -> Option<String> {
    match mapped {
        IrExpr::Ident(name) => Some(name.clone()),
        IrExpr::PartSelect { value, msb, lsb } => {
            let IrExpr::Ident(name) = value.as_ref() else {
                return None;
            };
            let hi = ir_try_eval_const_index_expr(msb)?;
            let lo = ir_try_eval_const_index_expr(lsb)?;
            let w_sel = (hi - lo).abs() as usize + 1;
            let w_port = child
                .ports
                .iter()
                .find(|p| p.name == child_lhs_port)
                .map(|p| p.width)
                .unwrap_or(0);
            let w_full = ir_net_width_in_module(parent, name);
            if w_sel == w_full && w_full > 0 && w_sel == w_port {
                Some(name.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn module_inlining(project: &mut IrProject) -> usize {
    let module_map: HashMap<String, IrModule> = project
        .modules
        .iter()
        .map(|m| (m.name.clone(), m.clone()))
        .collect();

    let mut total_inlined = 0;

    for parent in project.modules.iter_mut() {
        let mut new_assigns = Vec::new();
        let mut new_nets = Vec::new();
        let mut new_mem_arrays = Vec::new();
        let mut remaining_instances = Vec::new();

        for inst in std::mem::take(&mut parent.instances) {
            if let Some(child) = module_map.get(&inst.module_name) {
                let shape_ok = child.assigns.len() <= MAX_INLINE_ASSIGNS
                    && child.instances.is_empty()
                    && child.always_blocks.is_empty();
                if shape_ok && !child.initial_blocks.is_empty() {
                    remaining_instances.push(inst);
                    continue;
                }
                if shape_ok {
                    let prefix = format!("{}__", inst.instance_name);

                    // Build port→signal substitution map from connections.
                    // If the instance has .port(signal) connections, we
                    // replace the child's port references with the parent's
                    // signals, wiring up the hierarchy correctly.
                    let mut subst: HashMap<String, IrExpr> = HashMap::new();
                    for conn in &inst.connections {
                        if let Some(pn) = conn.port_name.as_ref() {
                            subst.insert(pn.clone(), conn.expr.clone());
                        }
                    }

                    for net in &child.nets {
                        if !subst.contains_key(&net.name) {
                            new_nets.push(IrNet {
                                name: format!("{}{}", prefix, net.name),
                                width: net.width,
                            });
                        }
                    }
                    for assign in &child.assigns {
                        let mut rhs = substitute_expr(&assign.rhs, &subst, &prefix);
                        let lhs = if let Some(mapped) = subst.get(&assign.lhs) {
                            // Any assign LHS listed in the port map is a submodule port. Do **not**
                            // require `direction == Some("output")`: legacy `module M(a,b)` headers leave
                            // `Port.direction` unset even when `output s;` appears in the body, and we
                            // would otherwise keep `prefix+s` nets that DCE drops (AddSub ripple).
                            if let IrExpr::PartSelect { value, msb, lsb } = mapped {
                                if let IrExpr::Ident(vec_name) = value.as_ref() {
                                    if let (Some(k_hi), Some(k_lo)) =
                                        (ir_try_eval_const_index_expr(msb), ir_try_eval_const_index_expr(lsb))
                                    {
                                        if k_hi == k_lo {
                                            let w_full = ir_net_width_in_module(parent, vec_name);
                                            let w_port = child
                                                .ports
                                                .iter()
                                                .find(|p| p.name == assign.lhs)
                                                .map(|p| p.width)
                                                .unwrap_or(1);
                                            if w_port == 1
                                                && w_full > 1
                                                && k_hi >= 0
                                                && (k_hi as usize) < w_full
                                            {
                                                rhs = ir_expr_merge_scalar_into_packed_vec(
                                                    vec_name,
                                                    k_hi,
                                                    rhs,
                                                    w_full,
                                                );
                                                vec_name.clone()
                                            } else {
                                                inline_drive_lhs(mapped, child, &assign.lhs, parent)
                                                    .unwrap_or_else(|| format!("{}{}", prefix, assign.lhs))
                                            }
                                        } else {
                                            inline_drive_lhs(mapped, child, &assign.lhs, parent)
                                                .unwrap_or_else(|| format!("{}{}", prefix, assign.lhs))
                                        }
                                    } else {
                                        inline_drive_lhs(mapped, child, &assign.lhs, parent)
                                            .unwrap_or_else(|| format!("{}{}", prefix, assign.lhs))
                                    }
                                } else {
                                    inline_drive_lhs(mapped, child, &assign.lhs, parent)
                                        .unwrap_or_else(|| format!("{}{}", prefix, assign.lhs))
                                }
                            } else {
                                inline_drive_lhs(mapped, child, &assign.lhs, parent)
                                    .unwrap_or_else(|| format!("{}{}", prefix, assign.lhs))
                            }
                        } else {
                            format!("{}{}", prefix, assign.lhs)
                        };
                        new_assigns.push(IrAssign { lhs, rhs, span: assign.span });
                    }
                    for ma in &child.mem_arrays {
                        new_mem_arrays.push(IrMemArray {
                            stem: format!("{}{}", prefix, ma.stem),
                            lo: ma.lo,
                            hi: ma.hi,
                            elem_width: ma.elem_width,
                        });
                    }
                    total_inlined += 1;
                    continue;
                }
            }
            remaining_instances.push(inst);
        }

        parent.instances = remaining_instances;
        parent.nets.extend(new_nets);
        parent.assigns.extend(new_assigns);
        parent.mem_arrays.extend(new_mem_arrays);
    }

    total_inlined
}

/// Substitute known port names with their parent-side expressions,
/// and prefix any remaining internal identifiers.
fn substitute_expr(
    expr: &IrExpr,
    subst: &HashMap<String, IrExpr>,
    prefix: &str,
) -> IrExpr {
    match expr {
        IrExpr::Ident(name) => {
            if let Some(replacement) = subst.get(name) {
                replacement.clone()
            } else {
                IrExpr::Ident(format!("{}{}", prefix, name))
            }
        }
        IrExpr::Const(v) => IrExpr::Const(*v),
        IrExpr::Binary { op, left, right } => IrExpr::Binary {
            op: *op,
            left: Box::new(substitute_expr(left, subst, prefix)),
            right: Box::new(substitute_expr(right, subst, prefix)),
        },
        IrExpr::Unary { op, operand } => IrExpr::Unary {
            op: *op,
            operand: Box::new(substitute_expr(operand, subst, prefix)),
        },
        IrExpr::Signed(inner) => IrExpr::Signed(Box::new(substitute_expr(inner, subst, prefix))),
        IrExpr::Ternary { cond, then_expr, else_expr } => IrExpr::Ternary {
            cond: Box::new(substitute_expr(cond, subst, prefix)),
            then_expr: Box::new(substitute_expr(then_expr, subst, prefix)),
            else_expr: Box::new(substitute_expr(else_expr, subst, prefix)),
        },
        IrExpr::Concat(exprs) => {
            IrExpr::Concat(exprs.iter().map(|e| substitute_expr(e, subst, prefix)).collect())
        }
        IrExpr::PartSelect { value, msb, lsb } => IrExpr::PartSelect {
            value: Box::new(substitute_expr(value, subst, prefix)),
            msb: Box::new(substitute_expr(msb, subst, prefix)),
            lsb: Box::new(substitute_expr(lsb, subst, prefix)),
        },
        IrExpr::MemRead { stem, index } => IrExpr::MemRead {
            stem: format!("{}{}", prefix, stem),
            index: Box::new(substitute_expr(index, subst, prefix)),
        },
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Peephole / instruction combining (paper #2 — among 29 essential passes)
// ═══════════════════════════════════════════════════════════════════════
//
// Multi-instruction pattern matching that combines several operations
// into fewer, cheaper equivalents. The Huang et al. paper identifies
// -instcombine as one of the frequently beneficial passes.

fn peephole(expr: &mut IrExpr) -> usize {
    let mut count = 0;

    // Bottom-up recursion
    match expr {
        IrExpr::Binary { left, right, .. } => {
            count += peephole(left);
            count += peephole(right);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += peephole(operand);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += peephole(cond);
            count += peephole(then_expr);
            count += peephole(else_expr);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += peephole(e);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += peephole(value);
            count += peephole(msb);
            count += peephole(lsb);
        }
        IrExpr::MemRead { index, .. } => {
            count += peephole(index);
        }
        IrExpr::Const(_) | IrExpr::Ident(_) => {}
    }

    if let Some(replacement) = try_peephole(expr) {
        *expr = replacement;
        count += 1;
    }
    count
}

fn try_peephole(expr: &IrExpr) -> Option<IrExpr> {
    // Ternary distribution checked first for all binary ops:
    // (cond ? a : b) op c → cond ? (a op c) : (b op c)
    // when both ternary arms and the other operand are constants.
    if let IrExpr::Binary { op, left, right } = expr {
        if let IrExpr::Ternary { cond, then_expr, else_expr } = left.as_ref() {
            if matches!(then_expr.as_ref(), IrExpr::Const(_))
                && matches!(else_expr.as_ref(), IrExpr::Const(_))
                && matches!(right.as_ref(), IrExpr::Const(_))
            {
                return Some(IrExpr::Ternary {
                    cond: cond.clone(),
                    then_expr: Box::new(IrExpr::Binary {
                        op: *op,
                        left: then_expr.clone(),
                        right: right.clone(),
                    }),
                    else_expr: Box::new(IrExpr::Binary {
                        op: *op,
                        left: else_expr.clone(),
                        right: right.clone(),
                    }),
                });
            }
        }
        // Symmetric: c op (cond ? a : b)
        if let IrExpr::Ternary { cond, then_expr, else_expr } = right.as_ref() {
            if matches!(then_expr.as_ref(), IrExpr::Const(_))
                && matches!(else_expr.as_ref(), IrExpr::Const(_))
                && matches!(left.as_ref(), IrExpr::Const(_))
            {
                return Some(IrExpr::Ternary {
                    cond: cond.clone(),
                    then_expr: Box::new(IrExpr::Binary {
                        op: *op,
                        left: left.clone(),
                        right: then_expr.clone(),
                    }),
                    else_expr: Box::new(IrExpr::Binary {
                        op: *op,
                        left: left.clone(),
                        right: else_expr.clone(),
                    }),
                });
            }
        }
    }

    match expr {
        // a + a → a << 1
        IrExpr::Binary { op: IrBinOp::Add, left, right } if left == right => {
            Some(IrExpr::Binary {
                op: IrBinOp::Shl,
                left: left.clone(),
                right: Box::new(IrExpr::Const(1)),
            })
        }

        // -a + b → b - a
        IrExpr::Binary { op: IrBinOp::Add, left, right } => {
            if let IrExpr::Unary { op: IrUnaryOp::Neg, operand } = left.as_ref() {
                return Some(IrExpr::Binary {
                    op: IrBinOp::Sub,
                    left: right.clone(),
                    right: operand.clone(),
                });
            }
            if let IrExpr::Unary { op: IrUnaryOp::Neg, operand } = right.as_ref() {
                return Some(IrExpr::Binary {
                    op: IrBinOp::Sub,
                    left: left.clone(),
                    right: operand.clone(),
                });
            }
            None
        }

        // (a << n) >> n → a & mask
        IrExpr::Binary { op: IrBinOp::Shr, left, right } => {
            if let IrExpr::Binary { op: IrBinOp::Shl, left: inner, right: shl_amt } = left.as_ref() {
                if shl_amt == right {
                    if let IrExpr::Const(n) = right.as_ref() {
                        if *n > 0 && *n < 64 {
                            let mask = (1i64 << (64 - n)) - 1;
                            return Some(IrExpr::Binary {
                                op: IrBinOp::And,
                                left: inner.clone(),
                                right: Box::new(IrExpr::Const(mask)),
                            });
                        }
                    }
                }
            }
            None
        }

        // a - (-b) → a + b
        IrExpr::Binary { op: IrBinOp::Sub, left, right } => {
            if let IrExpr::Unary { op: IrUnaryOp::Neg, operand } = right.as_ref() {
                return Some(IrExpr::Binary {
                    op: IrBinOp::Add,
                    left: left.clone(),
                    right: operand.clone(),
                });
            }
            None
        }

        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Code sinking (paper #2 — avoid computing values not needed)
// ═══════════════════════════════════════════════════════════════════════
//
// Huang et al. identify -sink as one of the frequently beneficial passes.
// For our IR (continuous assignments), the main application is ternary
// expressions where one arm's computation is wasted. We apply:
//
// 1. Ternary with identical subexpressions: factor out common parts.
// 2. Nested ternary with same condition: flatten.

fn sink_expr(expr: &mut IrExpr) -> usize {
    let mut count = 0;

    match expr {
        IrExpr::Binary { left, right, .. } => {
            count += sink_expr(left);
            count += sink_expr(right);
        }
        IrExpr::Unary { operand, .. } | IrExpr::Signed(operand) => {
            count += sink_expr(operand);
        }
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            count += sink_expr(cond);
            count += sink_expr(then_expr);
            count += sink_expr(else_expr);
        }
        IrExpr::Concat(exprs) => {
            for e in exprs.iter_mut() {
                count += sink_expr(e);
            }
        }
        IrExpr::PartSelect { value, msb, lsb } => {
            count += sink_expr(value);
            count += sink_expr(msb);
            count += sink_expr(lsb);
        }
        IrExpr::MemRead { index, .. } => {
            count += sink_expr(index);
        }
        IrExpr::Const(_) | IrExpr::Ident(_) => {}
    }

    if let Some(replacement) = try_sink(expr) {
        *expr = replacement;
        count += 1;
    }
    count
}

fn try_sink(expr: &IrExpr) -> Option<IrExpr> {
    match expr {
        // Nested ternary with same condition: c ? (c ? a : b) : e → c ? a : e
        IrExpr::Ternary { cond, then_expr, else_expr } => {
            if let IrExpr::Ternary {
                cond: inner_cond,
                then_expr: inner_then,
                ..
            } = then_expr.as_ref()
            {
                if cond == inner_cond {
                    return Some(IrExpr::Ternary {
                        cond: cond.clone(),
                        then_expr: inner_then.clone(),
                        else_expr: else_expr.clone(),
                    });
                }
            }
            // c ? a : (c ? b : e) → c ? a : e
            if let IrExpr::Ternary {
                cond: inner_cond,
                else_expr: inner_else,
                ..
            } = else_expr.as_ref()
            {
                if cond == inner_cond {
                    return Some(IrExpr::Ternary {
                        cond: cond.clone(),
                        then_expr: then_expr.clone(),
                        else_expr: inner_else.clone(),
                    });
                }
            }
            // Factor common op: c ? (a op x) : (b op x) → (c ? a : b) op x
            // Only when the common operand is NOT a constant, to avoid
            // fighting with peephole's ternary distribution (which pushes
            // constants into arms for folding).
            if let (
                IrExpr::Binary { op: op1, left: l1, right: r1 },
                IrExpr::Binary { op: op2, left: l2, right: r2 },
            ) = (then_expr.as_ref(), else_expr.as_ref())
            {
                if op1 == op2 {
                    if r1 == r2 && !matches!(r1.as_ref(), IrExpr::Const(_)) {
                        return Some(IrExpr::Binary {
                            op: *op1,
                            left: Box::new(IrExpr::Ternary {
                                cond: cond.clone(),
                                then_expr: l1.clone(),
                                else_expr: l2.clone(),
                            }),
                            right: r1.clone(),
                        });
                    }
                    if l1 == l2 && !matches!(l1.as_ref(), IrExpr::Const(_)) {
                        return Some(IrExpr::Binary {
                            op: *op1,
                            left: l1.clone(),
                            right: Box::new(IrExpr::Ternary {
                                cond: cond.clone(),
                                then_expr: r1.clone(),
                                else_expr: r2.clone(),
                            }),
                        });
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Loop unrolling (paper #2 — among most impactful passes for HLS)
// ═══════════════════════════════════════════════════════════════════════
//
// Huang et al. found that loop unrolling significantly reduces execution
// cycles by exposing ILP. For `always` blocks with `for` loops that have
// compile-time-known bounds, we fully unroll them.

const MAX_UNROLL_ITERATIONS: i64 = 256;

fn unroll_loops(block: &mut StmtBlock) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < block.len() {
        match &mut block.stmts_mut()[i] {
            IrStmt::IfElse { then_body, else_body, .. } => {
                count += unroll_loops(then_body);
                count += unroll_loops(else_body);
                i += 1;
            }
            IrStmt::Case { arms, default, .. } => {
                for arm in arms.iter_mut() {
                    count += unroll_loops(&mut arm.body);
                }
                count += unroll_loops(default);
                i += 1;
            }
            IrStmt::For { .. } => {
                let stmt = block.stmts_mut()[i].clone();
                let outer_span = block.span_of(i);
                if let IrStmt::For {
                    init_var,
                    init_val,
                    cond,
                    step_var,
                    step_expr,
                    body,
                } = stmt
                {
                    if let Some(unrolled) =
                        try_unroll(&init_var, &init_val, &cond, &step_var, &step_expr, &body)
                    {
                        // Splice unrolled statements into the parallel stmt + span vecs.
                        // Each unrolled copy inherits the outer For's span so the debugger
                        // highlights the original source on each iteration.
                        let n = unrolled.len();
                        block.stmts_mut().splice(i..=i, unrolled);
                        let span_vec = block.spans_mut();
                        span_vec.splice(i..=i, std::iter::repeat(outer_span).take(n));
                        count += 1;
                        continue;
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    count
}

fn try_unroll(
    init_var: &str,
    init_val: &IrExpr,
    cond: &IrExpr,
    step_var: &str,
    step_expr: &IrExpr,
    body: &StmtBlock,
) -> Option<Vec<IrStmt>> {
    if init_var != step_var {
        return None;
    }
    let start = match init_val {
        IrExpr::Const(v) => *v,
        _ => return None,
    };

    // Extract loop bound from condition: var < N, var <= N, var != N
    let (bound, inclusive) = match cond {
        IrExpr::Binary { op: IrBinOp::Lt, left, right } => {
            if let (IrExpr::Ident(v), IrExpr::Const(n)) = (left.as_ref(), right.as_ref()) {
                if v == init_var {
                    (*n, false)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        IrExpr::Binary { op: IrBinOp::Le, left, right } => {
            if let (IrExpr::Ident(v), IrExpr::Const(n)) = (left.as_ref(), right.as_ref()) {
                if v == init_var {
                    (*n, true)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        IrExpr::Binary { op: IrBinOp::Ne, left, right } => {
            if let (IrExpr::Ident(v), IrExpr::Const(n)) = (left.as_ref(), right.as_ref()) {
                if v == init_var {
                    (*n, false)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // Extract step increment: var + 1, var + N
    let step_inc = match step_expr {
        IrExpr::Binary { op: IrBinOp::Add, left, right } => {
            if let (IrExpr::Ident(v), IrExpr::Const(n)) = (left.as_ref(), right.as_ref()) {
                if v == init_var {
                    *n
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    if step_inc <= 0 {
        return None;
    }

    let end = if inclusive { bound + 1 } else { bound };
    let iterations = (end - start + step_inc - 1) / step_inc;

    if iterations < 0 || iterations > MAX_UNROLL_ITERATIONS {
        return None;
    }

    let mut result = Vec::new();
    for iter in 0..iterations {
        let val = start + iter * step_inc;
        for stmt in body {
            result.push(substitute_loop_var_in_stmt(stmt, init_var, val));
        }
    }
    Some(result)
}

fn substitute_loop_var_in_block(block: &StmtBlock, var: &str, val: i64) -> StmtBlock {
    let mut out = StmtBlock::with_capacity(block.len());
    for (s, sp) in block.iter_with_spans() {
        out.push(substitute_loop_var_in_stmt(s, var, val), sp);
    }
    out
}

fn substitute_loop_var_in_stmt(stmt: &IrStmt, var: &str, val: i64) -> IrStmt {
    match stmt {
        IrStmt::BlockingAssign { lhs, rhs } => IrStmt::BlockingAssign {
            lhs: lhs.clone(),
            rhs: substitute_loop_var(rhs, var, val),
        },
        IrStmt::NonBlockingAssign { lhs, rhs } => IrStmt::NonBlockingAssign {
            lhs: lhs.clone(),
            rhs: substitute_loop_var(rhs, var, val),
        },
        IrStmt::MemAssign {
            stem,
            index,
            rhs,
            nonblocking,
        } => IrStmt::MemAssign {
            stem: stem.clone(),
            index: substitute_loop_var(index, var, val),
            rhs: substitute_loop_var(rhs, var, val),
            nonblocking: *nonblocking,
        },
        IrStmt::IfElse { cond, then_body, else_body } => IrStmt::IfElse {
            cond: substitute_loop_var(cond, var, val),
            then_body: substitute_loop_var_in_block(then_body, var, val),
            else_body: substitute_loop_var_in_block(else_body, var, val),
        },
        IrStmt::Case { expr, arms, default } => IrStmt::Case {
            expr: substitute_loop_var(expr, var, val),
            arms: arms
                .iter()
                .map(|a| IrCaseArm {
                    value: substitute_loop_var(&a.value, var, val),
                    body: substitute_loop_var_in_block(&a.body, var, val),
                    care_mask: a.care_mask,
                })
                .collect(),
            default: substitute_loop_var_in_block(default, var, val),
        },
        IrStmt::For { .. } => stmt.clone(),
        IrStmt::Delay(_) | IrStmt::SystemTask { .. } => stmt.clone(),
    }
}

fn substitute_loop_var(expr: &IrExpr, var: &str, val: i64) -> IrExpr {
    match expr {
        IrExpr::Ident(name) if name == var => IrExpr::Const(val),
        IrExpr::Ident(_) | IrExpr::Const(_) => expr.clone(),
        IrExpr::Binary { op, left, right } => IrExpr::Binary {
            op: *op,
            left: Box::new(substitute_loop_var(left, var, val)),
            right: Box::new(substitute_loop_var(right, var, val)),
        },
        IrExpr::Unary { op, operand } => IrExpr::Unary {
            op: *op,
            operand: Box::new(substitute_loop_var(operand, var, val)),
        },
        IrExpr::Signed(inner) => IrExpr::Signed(Box::new(substitute_loop_var(inner, var, val))),
        IrExpr::Ternary { cond, then_expr, else_expr } => IrExpr::Ternary {
            cond: Box::new(substitute_loop_var(cond, var, val)),
            then_expr: Box::new(substitute_loop_var(then_expr, var, val)),
            else_expr: Box::new(substitute_loop_var(else_expr, var, val)),
        },
        IrExpr::Concat(exprs) => {
            IrExpr::Concat(exprs.iter().map(|e| substitute_loop_var(e, var, val)).collect())
        }
        IrExpr::PartSelect { value, msb, lsb } => IrExpr::PartSelect {
            value: Box::new(substitute_loop_var(value, var, val)),
            msb: Box::new(substitute_loop_var(msb, var, val)),
            lsb: Box::new(substitute_loop_var(lsb, var, val)),
        },
        IrExpr::MemRead { stem, index } => IrExpr::MemRead {
            stem: stem.clone(),
            index: Box::new(substitute_loop_var(index, var, val)),
        },
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Unit tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrBinOp, IrExpr, IrNet, IrUnaryOp};
    use crate::source_map::Span;
    use crate::Port;

    fn make_module(assigns: Vec<IrAssign>, ports: Vec<Port>) -> IrModule {
        IrModule {
            name: "test".into(),
            path: "test.v".into(),
            ports,
            nets: vec![],
            assigns,
            instances: vec![],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        }
    }

    fn port(name: &str) -> Port {
        Port { direction: Some("output".into()), name: name.into(), width: 1 }
    }

    fn c(v: i64) -> IrExpr { IrExpr::Const(v) }
    fn id(s: &str) -> IrExpr { IrExpr::Ident(s.into()) }

    fn bin(op: IrBinOp, l: IrExpr, r: IrExpr) -> IrExpr {
        IrExpr::Binary { op, left: Box::new(l), right: Box::new(r) }
    }

    fn unary(op: IrUnaryOp, e: IrExpr) -> IrExpr {
        IrExpr::Unary { op, operand: Box::new(e) }
    }

    // ── Constant folding ────────────────────────────────────────────

    #[test]
    fn fold_add_constants() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, c(3), c(5)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(8));
    }

    #[test]
    fn fold_nested_constants() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mul, bin(IrBinOp::Add, c(2), c(3)), c(4)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(20));
    }

    #[test]
    fn fold_unary_not() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: unary(IrUnaryOp::Not, c(0)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(-1));
    }

    #[test]
    fn fold_double_not() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: unary(IrUnaryOp::Not, unary(IrUnaryOp::Not, id("a"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn fold_double_neg() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: unary(IrUnaryOp::Neg, unary(IrUnaryOp::Neg, id("a"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    // ── Identity / annihilator ──────────────────────────────────────

    #[test]
    fn identity_add_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("a"), c(0)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn identity_mul_one() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mul, id("a"), c(1)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn annihilator_and_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::And, id("a"), c(0)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn annihilator_mul_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mul, id("a"), c(0)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn identity_or_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Or, c(0), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn xor_self_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Xor, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn sub_self_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Sub, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn and_self_is_self() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::And, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn or_self_is_self() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Or, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn div_self_is_one() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Div, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(1));
    }

    #[test]
    fn mod_self_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mod, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    // ── Comparison identities ───────────────────────────────────────

    #[test]
    fn eq_self_is_one() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Eq, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(1));
    }

    #[test]
    fn ne_self_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Ne, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn lt_self_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Lt, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    // ── Complement rules ────────────────────────────────────────────

    #[test]
    fn and_complement_is_zero() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::And, id("a"), unary(IrUnaryOp::Not, id("a"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn or_complement_is_all_ones() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Or, id("a"), unary(IrUnaryOp::Not, id("a"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(-1));
    }

    // ── De Morgan ───────────────────────────────────────────────────

    #[test]
    fn demorgan_not_and_constants() {
        // ~(3 & 5) should fold to ~(1) = ~1 = -2 via const fold
        // But De Morgan pushes in first: (~3 | ~5) → (-4 | -6) → (-4 | -6) = -2
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: unary(IrUnaryOp::Not, bin(IrBinOp::And, c(3), c(5))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(!1i64)); // 3 & 5 = 1, ~1 = -2
    }

    // ── Absorption ──────────────────────────────────────────────────

    #[test]
    fn absorption_and_or() {
        // a & (a | b) → a
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::And, id("a"), bin(IrBinOp::Or, id("a"), id("b"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn absorption_or_and() {
        // a | (a & b) → a
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Or, id("a"), bin(IrBinOp::And, id("a"), id("b"))), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    // ── Strength reduction ──────────────────────────────────────────

    #[test]
    fn strength_mul_power_of_2() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mul, id("a"), c(8)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Shl, id("a"), c(3)));
    }

    #[test]
    fn strength_div_power_of_2() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Div, id("a"), c(4)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Shr, id("a"), c(2)));
    }

    #[test]
    fn strength_mod_power_of_2() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mod, id("a"), c(16)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::And, id("a"), c(15)));
    }

    // ── Constant propagation ────────────────────────────────────────

    #[test]
    fn const_prop_simple() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "tmp".into(), rhs: c(42), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("tmp"), c(1)), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        assert_eq!(y.rhs, c(43));
    }

    // ── Copy propagation ────────────────────────────────────────────

    #[test]
    fn copy_prop_expr() {
        // t = a + 1, y = t + 2  →  y = a + 1 + 2  →  y = a + 3 (after fold)
        let mut m = make_module(
            vec![
                IrAssign { lhs: "t".into(), rhs: bin(IrBinOp::Add, id("a"), c(1)), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("t"), c(2)), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        // After copy prop and fold, we expect y = a + 3
        assert_eq!(y.rhs, bin(IrBinOp::Add, id("a"), c(3)));
    }

    #[test]
    fn alias_simple() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "b".into(), rhs: id("a"), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("b"), c(1)), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        assert_eq!(y.rhs, bin(IrBinOp::Add, id("a"), c(1)));
    }

    #[test]
    fn alias_chain() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "c_wire".into(), rhs: id("b_wire"), span: Span::dummy() },
                IrAssign { lhs: "b_wire".into(), rhs: id("a"), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("c_wire"), c(1)), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        assert_eq!(y.rhs, bin(IrBinOp::Add, id("a"), c(1)));
    }

    // ── CSE ─────────────────────────────────────────────────────────

    #[test]
    fn cse_dedup() {
        // t1 = a + b, t2 = a + b, y = t1 & t2
        // After CSE: t2 rewritten to use t1 → y = t1 & t1 → y = t1
        let mut m = make_module(
            vec![
                IrAssign { lhs: "t1".into(), rhs: bin(IrBinOp::Add, id("a"), id("b")), span: Span::dummy() },
                IrAssign { lhs: "t2".into(), rhs: bin(IrBinOp::Add, id("a"), id("b")), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::And, id("t1"), id("t2")), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        // After CSE + identity (x & x = x) + dead elim:
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        assert_eq!(y.rhs, bin(IrBinOp::Add, id("a"), id("b")));
    }

    // ── Dead signal elimination ─────────────────────────────────────

    #[test]
    fn dead_signal_removed() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "dead".into(), rhs: c(99), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: id("a"), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns.len(), 1);
        assert_eq!(m.assigns[0].lhs, "y");
    }

    #[test]
    fn port_signal_not_removed() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: c(5), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns.len(), 1);
    }

    // ── Ternary ─────────────────────────────────────────────────────

    #[test]
    fn ternary_true_folds() {
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(c(1)),
                    then_expr: Box::new(id("a")),
                    else_expr: Box::new(id("b")),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn ternary_false_folds() {
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(c(0)),
                    then_expr: Box::new(id("a")),
                    else_expr: Box::new(id("b")),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("b"));
    }

    #[test]
    fn ternary_same_arms() {
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(id("sel")),
                    then_expr: Box::new(id("a")),
                    else_expr: Box::new(id("a")),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    // ── Combined ────────────────────────────────────────────────────

    #[test]
    fn combined_optimization() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "tmp".into(), rhs: c(2), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Mul, id("tmp"), c(4)), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        assert_eq!(y.rhs, c(8));
    }

    #[test]
    fn shl_zero_identity() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Shl, id("a"), c(0)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, id("a"));
    }

    #[test]
    fn fold_eq_true() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Eq, c(7), c(7)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(1));
    }

    #[test]
    fn fold_lt_false() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Lt, c(10), c(3)), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    // ── Logical short-circuit ───────────────────────────────────────

    #[test]
    fn logand_zero_short_circuits() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::LogAnd, c(0), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(0));
    }

    #[test]
    fn logor_nonzero_short_circuits() {
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::LogOr, c(5), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, c(1));
    }

    // ── Canonicalization enables CSE ─────────────────────────────────

    #[test]
    fn canonical_enables_cse() {
        // t1 = a & b,  t2 = b & a  → after canonical both are a & b → CSE
        let mut m = make_module(
            vec![
                IrAssign { lhs: "t1".into(), rhs: bin(IrBinOp::And, id("a"), id("b")), span: Span::dummy() },
                IrAssign { lhs: "t2".into(), rhs: bin(IrBinOp::And, id("b"), id("a")), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Or, id("t1"), id("t2")), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let y = m.assigns.iter().find(|a| a.lhs == "y").unwrap();
        // t1 == t2 after CSE, so t1 | t1 = t1 = a & b
        assert_eq!(y.rhs, bin(IrBinOp::And, id("a"), id("b")));
    }

    // ── Peephole / instruction combining (paper #2) ────────────────

    #[test]
    fn peephole_add_self_to_shl1() {
        // a + a → a << 1
        let mut m = make_module(
            vec![IrAssign { lhs: "y".into(), rhs: bin(IrBinOp::Add, id("a"), id("a")), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Shl, id("a"), c(1)));
    }

    #[test]
    fn peephole_neg_a_plus_b() {
        // (-a) + b → b - a
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: bin(
                    IrBinOp::Add,
                    IrExpr::Unary { op: IrUnaryOp::Neg, operand: Box::new(id("a")) },
                    id("b"),
                ), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Sub, id("b"), id("a")));
    }

    #[test]
    fn peephole_a_plus_neg_b() {
        // a + (-b) → a - b
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: bin(
                    IrBinOp::Add,
                    id("a"),
                    IrExpr::Unary { op: IrUnaryOp::Neg, operand: Box::new(id("b")) },
                ), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Sub, id("a"), id("b")));
    }

    #[test]
    fn peephole_sub_neg() {
        // a - (-b) → a + b
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: bin(
                    IrBinOp::Sub,
                    id("a"),
                    IrExpr::Unary { op: IrUnaryOp::Neg, operand: Box::new(id("b")) },
                ), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::Add, id("a"), id("b")));
    }

    #[test]
    fn peephole_shl_shr_mask() {
        // (a << 3) >> 3 → a & mask
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: bin(
                    IrBinOp::Shr,
                    bin(IrBinOp::Shl, id("a"), c(3)),
                    c(3),
                ), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        let mask = (1i64 << 61) - 1;
        assert_eq!(m.assigns[0].rhs, bin(IrBinOp::And, id("a"), c(mask)));
    }

    #[test]
    fn peephole_ternary_distribution() {
        // (sel ? 3 : 5) + 2 → sel ? 5 : 7  (after fold)
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: bin(
                    IrBinOp::Add,
                    IrExpr::Ternary {
                        cond: Box::new(id("sel")),
                        then_expr: Box::new(c(3)),
                        else_expr: Box::new(c(5)),
                    },
                    c(2),
                ), span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        // After peephole distributes and then fold kicks in:
        // sel ? (3+2) : (5+2) = sel ? 5 : 7
        assert_eq!(
            m.assigns[0].rhs,
            IrExpr::Ternary {
                cond: Box::new(id("sel")),
                then_expr: Box::new(c(5)),
                else_expr: Box::new(c(7)),
            }
        );
    }

    // ── Code sinking (paper #2) ────────────────────────────────────

    #[test]
    fn sink_nested_ternary_same_cond_then() {
        // c ? (c ? a : b) : e → c ? a : e
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(id("c")),
                    then_expr: Box::new(IrExpr::Ternary {
                        cond: Box::new(id("c")),
                        then_expr: Box::new(id("a")),
                        else_expr: Box::new(id("b")),
                    }),
                    else_expr: Box::new(id("e")),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(
            m.assigns[0].rhs,
            IrExpr::Ternary {
                cond: Box::new(id("c")),
                then_expr: Box::new(id("a")),
                else_expr: Box::new(id("e")),
            }
        );
    }

    #[test]
    fn sink_nested_ternary_same_cond_else() {
        // c ? a : (c ? b : e) → c ? a : e
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(id("c")),
                    then_expr: Box::new(id("a")),
                    else_expr: Box::new(IrExpr::Ternary {
                        cond: Box::new(id("c")),
                        then_expr: Box::new(id("b")),
                        else_expr: Box::new(id("e")),
                    }),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(
            m.assigns[0].rhs,
            IrExpr::Ternary {
                cond: Box::new(id("c")),
                then_expr: Box::new(id("a")),
                else_expr: Box::new(id("e")),
            }
        );
    }

    #[test]
    fn sink_factor_common_right_operand() {
        // c ? (a + x) : (b + x) → (c ? a : b) + x
        // After canonicalization, idents sort before compound exprs, so result
        // may be x + (sel ? a : b).
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(id("sel")),
                    then_expr: Box::new(bin(IrBinOp::Add, id("a"), id("x"))),
                    else_expr: Box::new(bin(IrBinOp::Add, id("b"), id("x"))),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        // Sinking factors out x, and canonicalization may reorder.
        // Check that the result is Add with x and a ternary.
        match &m.assigns[0].rhs {
            IrExpr::Binary { op: IrBinOp::Add, left, right } => {
                let has_x = matches!(left.as_ref(), IrExpr::Ident(n) if n == "x")
                    || matches!(right.as_ref(), IrExpr::Ident(n) if n == "x");
                let has_ternary = matches!(left.as_ref(), IrExpr::Ternary { .. })
                    || matches!(right.as_ref(), IrExpr::Ternary { .. });
                assert!(has_x && has_ternary, "expected Add(x, ternary) in some order, got {:?}", m.assigns[0].rhs);
            }
            other => panic!("expected Binary Add, got {:?}", other),
        }
    }

    #[test]
    fn sink_factor_common_left_operand() {
        // c ? (x & a) : (x & b) → x & (c ? a : b)
        let mut m = make_module(
            vec![IrAssign {
                lhs: "y".into(),
                rhs: IrExpr::Ternary {
                    cond: Box::new(id("sel")),
                    then_expr: Box::new(bin(IrBinOp::And, id("x"), id("a"))),
                    else_expr: Box::new(bin(IrBinOp::And, id("x"), id("b"))),
                }, span: Span::dummy() }],
            vec![port("y")],
        );
        optimize_module(&mut m);
        assert_eq!(
            m.assigns[0].rhs,
            bin(
                IrBinOp::And,
                id("x"),
                IrExpr::Ternary {
                    cond: Box::new(id("sel")),
                    then_expr: Box::new(id("a")),
                    else_expr: Box::new(id("b")),
                },
            )
        );
    }

    // ── Module inlining (paper #2's #1 finding) ────────────────────

    #[test]
    fn module_inlining_inlines_small_child() {
        use crate::ir::{IrInstance, IrPortConn, IrProject};

        let child = IrModule {
            name: "inverter".into(),
            path: "test.v".into(),
            ports: vec![port("a"), port("y")],
            nets: vec![IrNet { name: "t".into(), width: 1 }],
            assigns: vec![
                IrAssign { lhs: "t".into(), rhs: IrExpr::Unary { op: IrUnaryOp::Not, operand: Box::new(id("a")) }, span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: id("t"), span: Span::dummy() },
            ],
            instances: vec![],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let parent = IrModule {
            name: "top".into(),
            path: "test.v".into(),
            ports: vec![port("x"), port("z")],
            nets: vec![],
            assigns: vec![],
            instances: vec![IrInstance {
                module_name: "inverter".into(),
                parameter_assignments: vec![],
                instance_name: "u0".into(),
                connections: vec![
                    IrPortConn {
                        port_name: Some("a".into()),
                        expr: id("x"),
                        span: Span::dummy(),
                    },
                    IrPortConn {
                        port_name: Some("y".into()),
                        expr: id("z"),
                        span: Span::dummy(),
                    },
                ],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let mut proj = IrProject {
            modules: vec![child, parent],
            diagnostics: vec![],
            source_map: SourceMap::new(),
        };
        let metrics = optimize_project(&mut proj);
        assert_eq!(metrics.modules_inlined, 1);
        let top = proj.modules.iter().find(|m| m.name == "top").unwrap();
        assert!(top.instances.is_empty(), "instance should be removed");
        // With port mapping: y is mapped to z, so assign z = ...
        assert!(top.assigns.iter().any(|a| a.lhs == "z"), "output port should be wired");
    }

    /// `c[i+1]` after generate lowering uses `IrExpr::Binary` indices; inlining must still RMW
    /// into the parent packed net (same requirement as codegen flatten).
    #[test]
    fn module_inline_merges_output_into_packed_vec_when_partselect_msb_is_binary_add() {
        use crate::ir::{IrInstance, IrPortConn, IrProject};

        let child = IrModule {
            name: "BitDrv".into(),
            path: "b.v".into(),
            ports: vec![
                Port {
                    direction: Some("input".into()),
                    name: "a".into(),
                    width: 1,
                },
                Port {
                    direction: Some("output".into()),
                    name: "cout".into(),
                    width: 1,
                },
            ],
            nets: vec![],
            assigns: vec![IrAssign {
                lhs: "cout".into(),
                rhs: id("a"), span: Span::dummy() }],
            instances: vec![],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let parent = IrModule {
            name: "Top".into(),
            path: "t.v".into(),
            ports: vec![],
            nets: vec![IrNet {
                name: "carry".into(),
                width: 5,
            }],
            assigns: vec![],
            instances: vec![IrInstance {
                module_name: "BitDrv".into(),
                instance_name: "u0".into(),
                parameter_assignments: vec![],
                connections: vec![
                    IrPortConn {
                        port_name: Some("a".into()),
                        expr: IrExpr::Const(1),
                        span: Span::dummy(),
                    },
                    IrPortConn {
                        port_name: Some("cout".into()),
                        expr: IrExpr::PartSelect {
                            value: Box::new(IrExpr::Ident("carry".into())),
                            msb: Box::new(bin(IrBinOp::Add, c(0), c(1))),
                            lsb: Box::new(bin(IrBinOp::Add, c(0), c(1))),
                        },
                        span: Span::dummy(),
                    },
                ],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let mut proj = IrProject {
            modules: vec![child, parent],
            diagnostics: vec![],
            source_map: SourceMap::new(),
        };
        let _ = optimize_project(&mut proj);
        let top = proj.modules.iter().find(|m| m.name == "Top").unwrap();
        assert!(top.instances.is_empty(), "instance should inline away");
        assert!(
            !top.assigns.iter().any(|a| a.lhs == "u0__cout"),
            "inlined 1-bit output must map into `carry`, not stay as prefixed net (assigns={:?})",
            top.assigns
        );
        assert!(
            top.assigns.iter().any(|a| a.lhs == "carry"),
            "expected driver onto parent packed net `carry`, assigns={:?}",
            top.assigns
        );
    }

    #[test]
    fn module_inlining_skips_large_modules() {
        use crate::ir::{IrInstance, IrProject};

        let big_assigns: Vec<IrAssign> = (0..20)
            .map(|i| IrAssign { lhs: format!("w{}", i), rhs: c(i), span: Span::dummy() })
            .collect();
        let child = IrModule {
            name: "big".into(),
            path: "test.v".into(),
            ports: vec![],
            nets: vec![],
            assigns: big_assigns,
            instances: vec![],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let parent = IrModule {
            name: "top".into(),
            path: "test.v".into(),
            ports: vec![],
            nets: vec![],
            assigns: vec![],
            instances: vec![IrInstance {
                module_name: "big".into(),
                parameter_assignments: vec![],
                instance_name: "u0".into(),
                connections: vec![],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let mut proj = IrProject {
            modules: vec![child, parent],
            diagnostics: vec![],
            source_map: SourceMap::new(),
        };
        let metrics = optimize_project(&mut proj);
        assert_eq!(metrics.modules_inlined, 0);
        let top = proj.modules.iter().find(|m| m.name == "top").unwrap();
        assert_eq!(top.instances.len(), 1, "large module should not be inlined");
    }

    #[test]
    fn module_inlining_skips_hierarchical_children() {
        use crate::ir::{IrInstance, IrProject};

        let child = IrModule {
            name: "has_sub".into(),
            path: "test.v".into(),
            ports: vec![],
            nets: vec![],
            assigns: vec![IrAssign { lhs: "w".into(), rhs: c(1), span: Span::dummy() }],
            instances: vec![IrInstance {
                module_name: "deep".into(),
                parameter_assignments: vec![],
                instance_name: "sub".into(),
                connections: vec![],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let parent = IrModule {
            name: "top".into(),
            path: "test.v".into(),
            ports: vec![],
            nets: vec![],
            assigns: vec![],
            instances: vec![IrInstance {
                module_name: "has_sub".into(),
                parameter_assignments: vec![],
                instance_name: "u0".into(),
                connections: vec![],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let mut proj = IrProject {
            modules: vec![child, parent],
            diagnostics: vec![],
            source_map: SourceMap::new(),
        };
        let metrics = optimize_project(&mut proj);
        assert_eq!(metrics.modules_inlined, 0);
    }

    #[test]
    fn module_inlining_skips_child_with_initial_blocks() {
        use crate::ir::{IrInitial, IrInstance, IrPortConn, IrProject, IrStmt};

        let child = IrModule {
            name: "with_init".into(),
            path: "test.v".into(),
            ports: vec![port("y")],
            nets: vec![IrNet { name: "t".into(), width: 1 }],
            assigns: vec![IrAssign {
                lhs: "y".into(),
                rhs: id("t"), span: Span::dummy() }],
            instances: vec![],
            always_blocks: vec![],
            initial_blocks: vec![IrInitial {
                stmts: StmtBlock::from(vec![IrStmt::BlockingAssign {
                    lhs: "t".into(),
                    rhs: c(1),
                }]),
            }],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let parent = IrModule {
            name: "top".into(),
            path: "test.v".into(),
            ports: vec![port("z")],
            nets: vec![],
            assigns: vec![],
            instances: vec![IrInstance {
                module_name: "with_init".into(),
                parameter_assignments: vec![],
                instance_name: "u0".into(),
                connections: vec![IrPortConn {
                    port_name: Some("y".into()),
                    expr: id("z"),
                    span: Span::dummy(),
                }],
            }],
            always_blocks: vec![],
            initial_blocks: vec![],
            mem_arrays: vec![],
            resolved_parameters: std::collections::HashMap::new(),
            file_id: SYNTHETIC_FILE,
        };
        let mut proj = IrProject {
            modules: vec![child, parent],
            diagnostics: vec![],
            source_map: SourceMap::new(),
        };
        let metrics = optimize_project(&mut proj);
        assert_eq!(metrics.modules_inlined, 0, "must not inline away initial_blocks");
        let top = proj.modules.iter().find(|m| m.name == "top").unwrap();
        assert_eq!(top.instances.len(), 1);
    }

    // ── Loop unrolling ─────────────────────────────────────────────

    #[test]
    fn loop_unrolling_simple_for() {
        // for (i = 0; i < 3; i = i + 1) out = i;
        // → out = 0; out = 1; out = 2;
        let mut stmts: StmtBlock = vec![IrStmt::For {
            init_var: "i".into(),
            init_val: c(0),
            cond: bin(IrBinOp::Lt, id("i"), c(3)),
            step_var: "i".into(),
            step_expr: bin(IrBinOp::Add, id("i"), c(1)),
            body: StmtBlock::from(vec![IrStmt::BlockingAssign {
                lhs: "out".into(),
                rhs: id("i"),
            }]),
        }]
        .into();
        let count = unroll_loops(&mut stmts);
        assert_eq!(count, 1);
        assert_eq!(stmts.len(), 3);
        // Each unrolled iteration replaces i with the constant
        if let IrStmt::BlockingAssign { rhs, .. } = &stmts[0] {
            assert_eq!(*rhs, c(0));
        }
        if let IrStmt::BlockingAssign { rhs, .. } = &stmts[2] {
            assert_eq!(*rhs, c(2));
        }
    }

    #[test]
    fn loop_unrolling_skips_unknown_bounds() {
        let mut stmts: StmtBlock = vec![IrStmt::For {
            init_var: "i".into(),
            init_val: c(0),
            cond: bin(IrBinOp::Lt, id("i"), id("n")), // n is not a constant
            step_var: "i".into(),
            step_expr: bin(IrBinOp::Add, id("i"), c(1)),
            body: StmtBlock::from(vec![IrStmt::BlockingAssign {
                lhs: "out".into(),
                rhs: id("i"),
            }]),
        }]
        .into();
        let count = unroll_loops(&mut stmts);
        assert_eq!(count, 0);
        assert_eq!(stmts.len(), 1); // For loop remains
    }

    // ── Adaptive scoring ───────────────────────────────────────────

    #[test]
    fn score_history_tracks_size() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "t".into(), rhs: bin(IrBinOp::Add, c(1), c(2)), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: id("t"), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        let metrics = optimize_module_with_metrics(&mut m);
        assert!(!metrics.score_history.is_empty());
        // After optimization, the final score should be <= the initial
        let first = metrics.score_history[0];
        let last = *metrics.score_history.last().unwrap();
        assert!(last <= first, "score should not increase");
    }

    // ── Metrics ────────────────────────────────────────────────────

    #[test]
    fn metrics_tracks_pass_counts() {
        let mut m = make_module(
            vec![
                IrAssign { lhs: "t".into(), rhs: bin(IrBinOp::Add, c(1), c(2)), span: Span::dummy() },
                IrAssign { lhs: "y".into(), rhs: id("t"), span: Span::dummy() },
            ],
            vec![port("y")],
        );
        let metrics = optimize_module_with_metrics(&mut m);
        assert!(metrics.algebraic_rewrites > 0, "should have folded constants");
        assert!(metrics.total_passes >= 1);
    }
}
