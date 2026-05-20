use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::delay_rational::DelayRational;
use crate::lexer;
use crate::parser::{
    self, AssignTarget, BinaryOp, CstBlock, CstModule, CstModuleItem, CstStmt, EdgeKind, Expr,
    Sensitivity, UnaryOp,
};
use crate::source_map::{FileId, Span, SourceMap, SYNTHETIC_FILE};
use crate::{Diagnostic, Port, SourceFile};

// ── Spanned block wrapper for IrStmt lists ─────────────────────────────
//
// Carries a parallel `Vec<Span>` alongside `Vec<IrStmt>` so the simulator can
// emit trace entries that point back at the originating byte range, without
// modifying every `IrStmt` variant's shape (and breaking every pattern match).
//
// Deref<Target=[IrStmt]> + IntoIterator let existing consumer code that does
// `for s in &block` or `&block[0]` continue to work unchanged.

#[derive(Debug, Clone, Default)]
pub struct StmtBlock {
    stmts: Vec<IrStmt>,
    spans: Vec<Span>,
}

impl StmtBlock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            stmts: Vec::with_capacity(n),
            spans: Vec::with_capacity(n),
        }
    }

    pub fn push(&mut self, stmt: IrStmt, span: Span) {
        self.stmts.push(stmt);
        self.spans.push(span);
    }

    pub fn stmts(&self) -> &[IrStmt] {
        &self.stmts
    }

    pub fn stmts_mut(&mut self) -> &mut Vec<IrStmt> {
        &mut self.stmts
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn spans_mut(&mut self) -> &mut Vec<Span> {
        &mut self.spans
    }

    pub fn span_of(&self, idx: usize) -> Span {
        self.spans
            .get(idx)
            .copied()
            .unwrap_or_else(Span::dummy)
    }

    pub fn len(&self) -> usize {
        self.stmts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stmts.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, IrStmt> {
        self.stmts.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, IrStmt> {
        self.stmts.iter_mut()
    }

    pub fn iter_with_spans(&self) -> impl Iterator<Item = (&IrStmt, Span)> + '_ {
        self.stmts.iter().zip(self.spans.iter().copied())
    }

    pub fn into_parts(self) -> (Vec<IrStmt>, Vec<Span>) {
        (self.stmts, self.spans)
    }

    pub fn from_parts(stmts: Vec<IrStmt>, spans: Vec<Span>) -> Self {
        debug_assert_eq!(stmts.len(), spans.len());
        Self { stmts, spans }
    }

    /// Build a block from a plain stmt vector, filling in dummy spans. Useful for
    /// call sites that synthesize statements (optimizer, prefix rewrites) and
    /// don't have a meaningful source location.
    pub fn with_dummy_spans(stmts: Vec<IrStmt>) -> Self {
        let spans = vec![Span::dummy(); stmts.len()];
        Self { stmts, spans }
    }
}

impl std::ops::Deref for StmtBlock {
    type Target = [IrStmt];
    fn deref(&self) -> &[IrStmt] {
        &self.stmts
    }
}

impl std::ops::DerefMut for StmtBlock {
    fn deref_mut(&mut self) -> &mut [IrStmt] {
        &mut self.stmts
    }
}

impl<'a> IntoIterator for &'a StmtBlock {
    type Item = &'a IrStmt;
    type IntoIter = std::slice::Iter<'a, IrStmt>;
    fn into_iter(self) -> Self::IntoIter {
        self.stmts.iter()
    }
}

impl<'a> IntoIterator for &'a mut StmtBlock {
    type Item = &'a mut IrStmt;
    type IntoIter = std::slice::IterMut<'a, IrStmt>;
    fn into_iter(self) -> Self::IntoIter {
        self.stmts.iter_mut()
    }
}

impl FromIterator<(IrStmt, Span)> for StmtBlock {
    fn from_iter<I: IntoIterator<Item = (IrStmt, Span)>>(iter: I) -> Self {
        let mut b = StmtBlock::new();
        for (s, sp) in iter {
            b.push(s, sp);
        }
        b
    }
}

/// Accept bare [`IrStmt`] collections (used in tests and legacy call sites that
/// don't track spans); every element gets a dummy span.
impl FromIterator<IrStmt> for StmtBlock {
    fn from_iter<I: IntoIterator<Item = IrStmt>>(iter: I) -> Self {
        let mut b = StmtBlock::new();
        for s in iter {
            b.push(s, Span::dummy());
        }
        b
    }
}

impl From<Vec<IrStmt>> for StmtBlock {
    fn from(stmts: Vec<IrStmt>) -> Self {
        StmtBlock::with_dummy_spans(stmts)
    }
}

// ── IR expression tree ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum IrExpr {
    Const(i64),
    Ident(String),
    Binary {
        op: IrBinOp,
        left: Box<IrExpr>,
        right: Box<IrExpr>,
    },
    Unary {
        op: IrUnaryOp,
        operand: Box<IrExpr>,
    },
    Ternary {
        cond: Box<IrExpr>,
        then_expr: Box<IrExpr>,
        else_expr: Box<IrExpr>,
    },
    Concat(Vec<IrExpr>),
    /// Packed part-select / bit-select: `value[msb:lsb]` (inclusive indices; LSB of value is bit 0).
    PartSelect {
        value: Box<IrExpr>,
        msb: Box<IrExpr>,
        lsb: Box<IrExpr>,
    },
    /// Unpacked array read: stem expands to `stem__lo`…`stem__hi` in the IR (see [`IrMemArray`]).
    MemRead {
        stem: String,
        index: Box<IrExpr>,
    },
    /// IEEE 1364 `$signed(expr)` — reinterpret packed result as signed using the inner expression width.
    Signed(Box<IrExpr>),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum IrBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    /// Verilog `>>>` (arithmetic shift right; sign extends per left operand width in simulation).
    Ashr,
    LogAnd,
    LogOr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum IrUnaryOp {
    Not,    // bitwise ~
    LogNot, // !
    Neg,    // unary -
}

/// `assign vec = (vec & ~(1<<k)) | ((scalar & 1) << k)` — one bit of a packed vector (IEEE 1364 RMW).
/// Used when inlining 1-bit submodule outputs into a parent slice (`S[i]`, `c[i+1]`, …).
pub(crate) fn ir_expr_merge_scalar_into_packed_vec(
    vec_name: &str,
    bit_k: i64,
    scalar_rhs: IrExpr,
    vec_width: usize,
) -> IrExpr {
    let w = vec_width.min(63).max(1);
    let width_mask = if w >= 63 { i64::MAX } else { (1i64 << w) - 1 };
    let k = bit_k.clamp(0, (w as i64).saturating_sub(1));
    let one_at_k = (1i64 << k) & width_mask;
    let clear_mask = width_mask ^ one_at_k;
    let old = IrExpr::Ident(vec_name.to_string());
    let cleared = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(old.clone()),
        right: Box::new(IrExpr::Const(clear_mask)),
    };
    let bit0 = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(scalar_rhs),
        right: Box::new(IrExpr::Const(1)),
    };
    let shifted = IrExpr::Binary {
        op: IrBinOp::Shl,
        left: Box::new(bit0),
        right: Box::new(IrExpr::Const(k)),
    };
    IrExpr::Binary {
        op: IrBinOp::Or,
        left: Box::new(cleared),
        right: Box::new(shifted),
    }
}

/// Part-select indices: `i+1` after generate unrolling are often [`IrExpr::Binary`], not [`IrExpr::Const`].
/// Returns [`None`] if the tree depends on a signal (identifier, mem read, nested part-select, etc.).
pub(crate) fn ir_try_eval_const_index_expr(e: &IrExpr) -> Option<i64> {
    fn binop(op: IrBinOp, l: i64, r: i64) -> Option<i64> {
        Some(match op {
            IrBinOp::Add => l.wrapping_add(r),
            IrBinOp::Sub => l.wrapping_sub(r),
            IrBinOp::Mul => l.wrapping_mul(r),
            IrBinOp::Div => {
                if r == 0 {
                    return None;
                }
                l.wrapping_div(r)
            }
            IrBinOp::Mod => {
                if r == 0 {
                    return None;
                }
                l.wrapping_rem(r)
            }
            IrBinOp::And => l & r,
            IrBinOp::Or => l | r,
            IrBinOp::Xor => l ^ r,
            IrBinOp::Shl => l.wrapping_shl((r as u32).min(63)),
            IrBinOp::Shr => ((l as u64).wrapping_shr((r as u32).min(63))) as i64,
            IrBinOp::Ashr => crate::arith::arith_shr_i64(l, (r as u32).min(63), 64),
            IrBinOp::LogAnd => i64::from(l != 0 && r != 0),
            IrBinOp::LogOr => i64::from(l != 0 || r != 0),
            IrBinOp::Eq => i64::from(l == r),
            IrBinOp::Ne => i64::from(l != r),
            IrBinOp::Lt => i64::from(l < r),
            IrBinOp::Le => i64::from(l <= r),
            IrBinOp::Gt => i64::from(l > r),
            IrBinOp::Ge => i64::from(l >= r),
        })
    }
    fn unop(op: IrUnaryOp, v: i64) -> i64 {
        match op {
            IrUnaryOp::Not => !v,
            IrUnaryOp::LogNot => i64::from(v == 0),
            IrUnaryOp::Neg => v.wrapping_neg(),
        }
    }
    match e {
        IrExpr::Const(c) => Some(*c),
        IrExpr::Binary { op, left, right } => {
            let l = ir_try_eval_const_index_expr(left)?;
            let r = ir_try_eval_const_index_expr(right)?;
            binop(*op, l, r)
        }
        IrExpr::Unary { op, operand } => {
            let v = ir_try_eval_const_index_expr(operand)?;
            Some(unop(*op, v))
        }
        IrExpr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            let c = ir_try_eval_const_index_expr(cond)?;
            if c != 0 {
                ir_try_eval_const_index_expr(then_expr)
            } else {
                ir_try_eval_const_index_expr(else_expr)
            }
        }
        IrExpr::Signed(inner) => {
            let v = ir_try_eval_const_index_expr(inner)?;
            Some(crate::arith::sign_extend_i64(v, 32))
        }
        IrExpr::Ident(_)
        | IrExpr::Concat(_)
        | IrExpr::PartSelect { .. }
        | IrExpr::MemRead { .. } => None,
    }
}

pub(crate) fn ir_net_width_in_module(m: &IrModule, name: &str) -> usize {
    m.ports
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.width)
        .or_else(|| m.nets.iter().find(|n| n.name == name).map(|n| n.width))
        .unwrap_or(0)
}

// ── IR project / module structures ──────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct IrProject {
    pub modules: Vec<IrModule>,
    pub diagnostics: Vec<Diagnostic>,
    /// Registry of source files + precomputed line starts; keyed by [`FileId`].
    /// Populated by `build_ir_for_*` so debugger/trace consumers can resolve spans.
    pub source_map: SourceMap,
}

#[derive(Debug, Clone)]
pub struct IrModule {
    pub name: String,
    pub path: String,
    /// Registered file-id for [`IrModule::path`] in the owning [`IrProject::source_map`].
    /// `SYNTHETIC_FILE` for modules synthesized after elaboration without a concrete file.
    pub file_id: FileId,
    pub ports: Vec<Port>,
    pub nets: Vec<IrNet>,
    pub assigns: Vec<IrAssign>,
    pub instances: Vec<IrInstance>,
    pub always_blocks: Vec<IrAlways>,
    pub initial_blocks: Vec<IrInitial>,
    /// Unpacked `reg [...] name[a:b]` arrays lowered to `name__k` scalars; used by [`IrExpr::MemRead`].
    pub mem_arrays: Vec<IrMemArray>,
    /// Resolved `parameter` / `localparam` values for this module (used to evaluate child `#(.param(expr))`).
    pub resolved_parameters: HashMap<String, i64>,
}

impl Default for IrModule {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            file_id: SYNTHETIC_FILE,
            ports: Vec::new(),
            nets: Vec::new(),
            assigns: Vec::new(),
            instances: Vec::new(),
            always_blocks: Vec::new(),
            initial_blocks: Vec::new(),
            mem_arrays: Vec::new(),
            resolved_parameters: HashMap::new(),
        }
    }
}

/// Metadata for unpacked arrays declared as `reg [w-1:0] stem[hi:lo];`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrMemArray {
    pub stem: String,
    pub lo: i64,
    pub hi: i64,
    pub elem_width: usize,
}

#[derive(Debug, Clone)]
pub struct IrNet {
    pub name: String,
    pub width: usize,
}

#[derive(Debug, Clone, Default)]
pub struct IrInitial {
    pub stmts: StmtBlock,
}

#[derive(Debug, Clone)]
pub struct IrAssign {
    pub lhs: String,
    pub rhs: IrExpr,
    /// Byte span of the originating `assign … ;` statement in the owning
    /// [`IrModule`]'s source file. [`Span::dummy`] when synthesized (e.g. by
    /// the optimizer) or lowered without source information.
    pub span: Span,
}

impl Default for IrAssign {
    fn default() -> Self {
        Self {
            lhs: String::new(),
            rhs: IrExpr::Const(0),
            span: Span::dummy(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IrInstance {
    pub module_name: String,
    /// `#(.param(ir_expr), …)` lowered in the **parent**'s environment; cleared after elaboration.
    pub parameter_assignments: Vec<(String, IrExpr)>,
    pub instance_name: String,
    pub connections: Vec<IrPortConn>,
}

/// Port connection: named (`.p(e)`) or positional before [`resolve_instance_port_connections`].
#[derive(Debug, Clone)]
pub struct IrPortConn {
    pub port_name: Option<String>,
    pub expr: IrExpr,
    /// Source span of the `.port(expr)` connection in the instantiating
    /// module (or the bare `expr` for positional connections). Used by
    /// `flatten_module` to attach a real source location to the glue
    /// `assign` it synthesizes for instance-port drives — without this,
    /// "Jump to Driver" on a submodule-input port has no span to resolve.
    /// `Span::dummy()` when unknown (synthesized at elaboration time).
    pub span: Span,
}

// ── Sequential / procedural IR ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrEdgeKind {
    Posedge,
    Negedge,
    Level,
}

#[derive(Debug, Clone)]
pub struct IrSensEntry {
    pub edge: IrEdgeKind,
    pub signal: String,
}

#[derive(Debug, Clone)]
pub enum IrSensitivity {
    Star,
    EdgeList(Vec<IrSensEntry>),
}

#[derive(Debug, Clone)]
pub struct IrAlways {
    pub sensitivity: IrSensitivity,
    pub stmts: StmtBlock,
}

#[derive(Debug, Clone)]
pub enum IrStmt {
    BlockingAssign { lhs: String, rhs: IrExpr },
    NonBlockingAssign { lhs: String, rhs: IrExpr },
    /// Unpacked memory write: `mem[idx] <= rhs` (or blocking) when `idx` is not a compile-time constant.
    MemAssign {
        stem: String,
        index: IrExpr,
        rhs: IrExpr,
        nonblocking: bool,
    },
    IfElse {
        cond: IrExpr,
        then_body: StmtBlock,
        else_body: StmtBlock,
    },
    Case {
        expr: IrExpr,
        arms: Vec<IrCaseArm>,
        default: StmtBlock,
    },
    For {
        init_var: String,
        init_val: IrExpr,
        cond: IrExpr,
        step_var: String,
        step_expr: IrExpr,
        body: StmtBlock,
    },
    Delay(DelayRational),
    SystemTask {
        name: String,
        args: Vec<IrExpr>,
    },
}

#[derive(Debug, Clone)]
pub struct IrCaseArm {
    pub value: IrExpr,
    pub body: StmtBlock,
    /// When `Some(mask)`, the case-arm match is `(scrutinee & mask) ==
    /// (value & mask)` (used by `casez` / `casex` for `?` / `z` / `x`
    /// wildcards in the literal). `None` means exact-equality, the legacy
    /// `case` semantics.
    pub care_mask: Option<i64>,
}

// ── Public API ──────────────────────────────────────────────────────

pub fn build_ir_for_file(path: impl Into<String>, content: &str) -> IrProject {
    let path: String = path.into();
    let file = SourceFile::new(path.clone(), content);
    let tokens = lexer::lex(&file);
    let (cst, diagnostics) = parser::parse_cst(&file, &tokens);
    let mut source_map = SourceMap::new();
    let file_id = source_map.intern(&path, content);
    let mut cst_map: HashMap<String, CstModule> = HashMap::new();
    let mut modules = Vec::new();
    for m in cst.modules {
        cst_map.insert(m.name.clone(), m.clone());
        modules.push(ir_module_from_cst(m, file_id));
    }
    let mut project = IrProject {
        modules,
        diagnostics,
        source_map,
    };
    elaborate_parameterized_modules(&mut project, &cst_map);
    project
}

/// Parse and lower several Verilog files into one [`IrProject`], with **one** parameterized
/// elaboration pass so instances can reference child modules defined in other files.
pub fn build_ir_for_path_bufs(paths: &[std::path::PathBuf]) -> std::io::Result<IrProject> {
    let mut all_modules = Vec::new();
    let mut all_diags = Vec::new();
    let mut cst_map: HashMap<String, CstModule> = HashMap::new();
    let mut source_map = SourceMap::new();
    for path in paths {
        let src = std::fs::read_to_string(path)?;
        let path_s = path.to_string_lossy().into_owned();
        let file = SourceFile::new(&path_s, &src);
        let tokens = lexer::lex(&file);
        let (cst, mut diags) = parser::parse_cst(&file, &tokens);
        all_diags.append(&mut diags);
        let file_id = source_map.intern(&path_s, &src);
        for m in cst.modules {
            cst_map.insert(m.name.clone(), m.clone());
            all_modules.push(ir_module_from_cst(m, file_id));
        }
    }
    let mut project = IrProject {
        modules: all_modules,
        diagnostics: all_diags,
        source_map,
    };
    elaborate_parameterized_modules(&mut project, &cst_map);
    Ok(project)
}

pub fn build_ir_for_root(root: &Path) -> std::io::Result<IrProject> {
    let mut all_modules = Vec::new();
    let mut all_diags = Vec::new();
    let mut cst_map: HashMap<String, CstModule> = HashMap::new();
    let mut source_map = SourceMap::new();
    walk_dir(root, &mut |path| {
        if let Ok(src) = std::fs::read_to_string(path) {
            let path_s = path.to_string_lossy().into_owned();
            let file = SourceFile::new(&path_s, &src);
            let tokens = lexer::lex(&file);
            let (cst, mut diags) = parser::parse_cst(&file, &tokens);
            all_diags.append(&mut diags);
            let file_id = source_map.intern(&path_s, &src);
            for m in cst.modules {
                cst_map.insert(m.name.clone(), m.clone());
                all_modules.push(ir_module_from_cst(m, file_id));
            }
        }
    })?;
    let mut project = IrProject {
        modules: all_modules,
        diagnostics: all_diags,
        source_map,
    };
    elaborate_parameterized_modules(&mut project, &cst_map);
    Ok(project)
}

/// Map positional instance ports (`port_name: None`) to the child module's port names in
/// declaration order. Call on a merged [`IrProject`] before optimization / simulation.
pub fn resolve_instance_port_connections(project: &mut IrProject) -> Result<(), String> {
    let child_ports_by_module: HashMap<String, Vec<Port>> = project
        .modules
        .iter()
        .map(|m| (m.name.clone(), m.ports.clone()))
        .collect();
    for module in &mut project.modules {
        for inst in &mut module.instances {
            let Some(child_ports) = child_ports_by_module.get(&inst.module_name) else {
                continue;
            };
            let has_pos = inst.connections.iter().any(|c| c.port_name.is_none());
            let has_named = inst.connections.iter().any(|c| c.port_name.is_some());
            if has_pos && has_named {
                return Err(format!(
                    "module `{}` instance `{}` of `{}`: mixed positional and named port connections are not supported",
                    module.name, inst.instance_name, inst.module_name
                ));
            }
            if !has_pos {
                continue;
            }
            if inst.connections.len() > child_ports.len() {
                return Err(format!(
                    "module `{}` instance `{}` of `{}`: too many positional ports ({} > {})",
                    module.name,
                    inst.instance_name,
                    inst.module_name,
                    inst.connections.len(),
                    child_ports.len()
                ));
            }
            for (i, c) in inst.connections.iter_mut().enumerate() {
                if c.port_name.is_none() {
                    let p = child_ports.get(i).ok_or_else(|| {
                        format!(
                            "module `{}` instance `{}` of `{}`: positional port index {} out of range",
                            module.name, inst.instance_name, inst.module_name, i
                        )
                    })?;
                    c.port_name = Some(p.name.clone());
                }
            }
        }
    }
    Ok(())
}

/// Evaluate `IrExpr` for `#(.param(expr))` using the **parent** module's resolved parameters.
pub(crate) fn eval_ir_param_expr(e: &IrExpr, known: &HashMap<String, i64>) -> Option<i64> {
    match e {
        IrExpr::Const(c) => Some(*c),
        IrExpr::Ident(name) => known.get(name).copied(),
        IrExpr::Binary { op, left, right } => {
            let l = eval_ir_param_expr(left, known)?;
            let r = eval_ir_param_expr(right, known)?;
            Some(match op {
                IrBinOp::Add => l.wrapping_add(r),
                IrBinOp::Sub => l.wrapping_sub(r),
                IrBinOp::Mul => l.wrapping_mul(r),
                IrBinOp::Div => {
                    if r == 0 {
                        0
                    } else {
                        l.wrapping_div(r)
                    }
                }
                IrBinOp::Mod => {
                    if r == 0 {
                        0
                    } else {
                        l.wrapping_rem(r)
                    }
                }
                IrBinOp::And => l & r,
                IrBinOp::Or => l | r,
                IrBinOp::Xor => l ^ r,
                IrBinOp::Shl => l.wrapping_shl((r as u32).min(63)),
                IrBinOp::Shr => l.wrapping_shr((r as u32).min(63)),
                IrBinOp::Ashr => crate::arith::arith_shr_i64(l, (r as u32).min(63), 64),
                IrBinOp::LogAnd => i64::from(l != 0 && r != 0),
                IrBinOp::LogOr => i64::from(l != 0 || r != 0),
                IrBinOp::Eq => i64::from(l == r),
                IrBinOp::Ne => i64::from(l != r),
                IrBinOp::Lt => i64::from(l < r),
                IrBinOp::Le => i64::from(l <= r),
                IrBinOp::Gt => i64::from(l > r),
                IrBinOp::Ge => i64::from(l >= r),
            })
        }
        IrExpr::Unary { op, operand } => {
            let v = eval_ir_param_expr(operand, known)?;
            Some(match op {
                IrUnaryOp::Neg => v.wrapping_neg(),
                IrUnaryOp::Not => !v,
                IrUnaryOp::LogNot => i64::from(v == 0),
            })
        }
        IrExpr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            let c = eval_ir_param_expr(cond, known)?;
            if c != 0 {
                eval_ir_param_expr(then_expr, known)
            } else {
                eval_ir_param_expr(else_expr, known)
            }
        }
        IrExpr::Signed(inner) => {
            let v = eval_ir_param_expr(inner, known)?;
            Some(crate::arith::sign_extend_i64(v, 32))
        }
        IrExpr::Concat(_)
        | IrExpr::PartSelect { .. }
        | IrExpr::MemRead { .. } => None,
    }
}

fn specialized_module_name(base: &str, sorted_pairs: &[(String, i64)]) -> String {
    use std::fmt::Write;
    let mut s = format!("{}__p", base);
    for (k, v) in sorted_pairs {
        let _ = write!(s, "_{}_{}", k, v);
    }
    s
}

/// For each instance with `#(.param(expr), …)`, clone the child [`CstModule`], apply numeric
/// parameter overrides, lower to a uniquely named [`IrModule`], and rewrite the instance.
pub fn elaborate_parameterized_modules(
    project: &mut IrProject,
    cst_map: &HashMap<String, CstModule>,
) {
    let initial_len = project.modules.len();
    let mut cache: HashMap<(String, Vec<(String, i64)>), String> = HashMap::new();
    for mi in 0..initial_len {
        let parent_params = project.modules[mi].resolved_parameters.clone();
        let parent_file_id = project.modules[mi].file_id;
        let instances = std::mem::take(&mut project.modules[mi].instances);
        let mut new_insts = Vec::with_capacity(instances.len());
        for mut inst in instances {
            if inst.parameter_assignments.is_empty() {
                new_insts.push(inst);
                continue;
            }
            let base = inst.module_name.clone();
            let mut eval_map: HashMap<String, i64> = HashMap::new();
            let mut ok = true;
            for (pname, ir_e) in &inst.parameter_assignments {
                match eval_ir_param_expr(ir_e, &parent_params) {
                    Some(v) => {
                        eval_map.insert(pname.clone(), v);
                    }
                    None => ok = false,
                }
            }
            if !ok {
                new_insts.push(inst);
                continue;
            }
            let mut pairs: Vec<_> = eval_map.iter().map(|(k, v)| (k.clone(), *v)).collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let cache_key = (base.clone(), pairs.clone());
            let spec_name = if let Some(n) = cache.get(&cache_key) {
                n.clone()
            } else {
                let Some(template) = cst_map.get(&base) else {
                    new_insts.push(inst);
                    continue;
                };
                let mut specialized = template.clone();
                for (pk, pv) in &eval_map {
                    if let Some((_, e)) = specialized
                        .module_parameters
                        .iter_mut()
                        .find(|(n, _)| n == pk)
                    {
                        *e = Expr::Number(format!("{}", pv));
                    }
                }
                let spec_name = specialized_module_name(&base, &pairs);
                specialized.name = spec_name.clone();
                let ir_mod = ir_module_from_cst(specialized, parent_file_id);
                cache.insert(cache_key, spec_name.clone());
                project.modules.push(ir_mod);
                spec_name
            };
            inst.module_name = spec_name;
            inst.parameter_assignments.clear();
            new_insts.push(inst);
        }
        project.modules[mi].instances = new_insts;
    }
}

// ── Lowering from CST to IR ─────────────────────────────────────────

fn collect_mem_stems(items: &[CstModuleItem]) -> HashSet<String> {
    let mut s = HashSet::new();
    for item in items {
        if let CstModuleItem::NetDecl { unpacked_stems, .. } = item {
            for (stem, _, _) in unpacked_stems {
                s.insert(stem.clone());
            }
        }
    }
    s
}

fn collect_net_widths(
    items: &[CstModuleItem],
    locals: &std::collections::HashMap<String, i64>,
) -> HashMap<String, usize> {
    use crate::expr_const::const_eval_param_expr;
    let mut m = HashMap::new();
    for item in items {
        if let CstModuleItem::NetDecl {
            packed_dim,
            width,
            names,
            unpacked_stems,
            ..
        } = item
        {
            let w_scalar = if let Some((msb, lsb)) = packed_dim.as_ref() {
                let m = const_eval_param_expr(msb, locals).unwrap_or(1);
                let l = const_eval_param_expr(lsb, locals).unwrap_or(1);
                ((m - l).abs() as usize).saturating_add(1).max(1)
            } else {
                *width
            };
            for n in names {
                if unpacked_stems.iter().any(|(s, _, _)| s == n) {
                    continue;
                }
                m.insert(n.clone(), w_scalar);
            }
            for (stem, hi_e, lo_e) in unpacked_stems.iter() {
                let hi = const_eval_param_expr(hi_e, locals).unwrap_or(0);
                let lo = const_eval_param_expr(lo_e, locals).unwrap_or(0);
                let low = hi.min(lo);
                let high = hi.max(lo);
                for i in low..=high {
                    m.insert(format!("{}__{}", stem, i), w_scalar);
                }
            }
        }
    }
    m
}

fn collect_mem_arrays(
    items: &[CstModuleItem],
    locals: &std::collections::HashMap<String, i64>,
) -> Vec<IrMemArray> {
    use crate::expr_const::const_eval_param_expr;
    let mut v = Vec::new();
    for item in items {
        if let CstModuleItem::NetDecl {
            packed_dim,
            width,
            unpacked_stems,
            ..
        } = item
        {
            let w_scalar = if let Some((msb, lsb)) = packed_dim.as_ref() {
                let m = const_eval_param_expr(msb, locals).unwrap_or(1);
                let l = const_eval_param_expr(lsb, locals).unwrap_or(1);
                ((m - l).abs() as usize).saturating_add(1).max(1)
            } else {
                *width
            };
            for (stem, hi_e, lo_e) in unpacked_stems.iter() {
                let hi = const_eval_param_expr(hi_e, locals).unwrap_or(0);
                let lo = const_eval_param_expr(lo_e, locals).unwrap_or(0);
                let low = hi.min(lo);
                let high = hi.max(lo);
                v.push(IrMemArray {
                    stem: stem.clone(),
                    lo: low,
                    hi: high,
                    elem_width: w_scalar,
                });
            }
        }
    }
    v
}

fn collect_local_param_assignments(items: &[CstModuleItem]) -> Vec<(String, Expr)> {
    let mut v = Vec::new();
    for item in items {
        if let CstModuleItem::LocalParam { assignments } = item {
            v.extend(assignments.iter().cloned());
        }
    }
    v
}

/// Resolved `parameter` / `localparam` environment for a CST module (used by semantic + lowering).
pub(crate) fn module_locals_for_cst(cst: &CstModule) -> std::collections::HashMap<String, i64> {
    let mut param_pairs = cst.module_parameters.clone();
    param_pairs.extend(collect_local_param_assignments(&cst.items));
    crate::expr_const::resolve_local_param_values(&param_pairs)
}

/// Non-ANSI `module M(a,b)` headers leave [`Port::direction`] unset; merge `input` / `output` /
/// `inout` declarations from the module body so hierarchy and inlining know port directions.
fn merge_port_directions_from_body(items: &[CstModuleItem], ports: &mut [Port]) {
    for item in items {
        let CstModuleItem::NetDecl {
            decl_dir: Some(dir),
            names,
            ..
        } = item
        else {
            continue;
        };
        for n in names {
            if let Some(p) = ports.iter_mut().find(|p| p.name == *n) {
                p.direction = Some(dir.clone());
            }
        }
    }
}

fn ir_module_from_cst(mut cst: CstModule, file_id: FileId) -> IrModule {
    merge_port_directions_from_body(&cst.items, &mut cst.ports);
    let mut param_pairs = cst.module_parameters.clone();
    param_pairs.extend(collect_local_param_assignments(&cst.items));
    let locals = crate::expr_const::resolve_local_param_values(&param_pairs);
    let mem_stems = collect_mem_stems(&cst.items);
    let mem_arrays = collect_mem_arrays(&cst.items, &locals);
    let net_widths = collect_net_widths(&cst.items, &locals);
    let mut assigns = Vec::new();
    let mut nets = Vec::new();
    let mut instances = Vec::new();
    let mut always_blocks = Vec::new();
    let mut initial_blocks = Vec::new();
    for item in cst.items {
        match item {
            CstModuleItem::Assign { target, expr, range } => {
                if let Some(a) = lower_continuous_assign(
                    target,
                    expr,
                    &mem_stems,
                    &locals,
                    &net_widths,
                    file_id,
                    range,
                ) {
                    assigns.push(a);
                }
            }
            CstModuleItem::NetDecl {
                packed_dim,
                width,
                names,
                unpacked_stems,
                ..
            } => {
                use crate::expr_const::const_eval_param_expr;
                let w_scalar = if let Some((msb, lsb)) = packed_dim {
                    let m = const_eval_param_expr(&msb, &locals).unwrap_or(1);
                    let l = const_eval_param_expr(&lsb, &locals).unwrap_or(1);
                    ((m - l).abs() as usize).saturating_add(1).max(1)
                } else {
                    width
                };
                for n in names {
                    if unpacked_stems.iter().any(|(s, _, _)| s == &n) {
                        continue;
                    }
                    nets.push(IrNet {
                        name: n,
                        width: w_scalar,
                    });
                }
                for (stem, hi_e, lo_e) in unpacked_stems.iter() {
                    let hi = const_eval_param_expr(hi_e, &locals).unwrap_or(0);
                    let lo = const_eval_param_expr(lo_e, &locals).unwrap_or(0);
                    let low = hi.min(lo);
                    let high = hi.max(lo);
                    for i in low..=high {
                        nets.push(IrNet {
                            name: format!("{}__{}", stem, i),
                            width: w_scalar,
                        });
                    }
                }
            }
            CstModuleItem::Instance {
                module_name,
                parameter_assignments,
                instance_name,
                connections,
            } => {
                let param_ir: Vec<(String, IrExpr)> = parameter_assignments
                    .into_iter()
                    .map(|(n, e)| (n, lower_expr(e, &mem_stems, &locals)))
                    .collect();
                let conns = connections
                    .into_iter()
                    .map(|c| IrPortConn {
                        port_name: c.port_name,
                        span: if c.range.1 > c.range.0 {
                            Span::new(file_id, c.range.0, c.range.1)
                        } else {
                            Span::dummy()
                        },
                        expr: lower_expr(c.expr, &mem_stems, &locals),
                    })
                    .collect();
                instances.push(IrInstance {
                    module_name,
                    parameter_assignments: param_ir,
                    instance_name,
                    connections: conns,
                });
            }
            CstModuleItem::GenerateFor {
                loop_var,
                upper_expr,
                module_name,
                parameter_assignments,
                instance_stem,
                connections,
            } => {
                use crate::expr_const::const_eval_param_expr;
                use crate::parser::subst_port_connections;
                let n = const_eval_param_expr(&upper_expr, &locals)
                    .unwrap_or(0)
                    .max(0) as usize;
                for k in 0..n {
                    let conns = subst_port_connections(&connections, &loop_var, k as i64);
                    let param_ir: Vec<(String, IrExpr)> = parameter_assignments
                        .iter()
                        .cloned()
                        .map(|(n, e)| (n, lower_expr(e, &mem_stems, &locals)))
                        .collect();
                    let conns_ir = conns
                        .into_iter()
                        .map(|c| IrPortConn {
                            port_name: c.port_name,
                            span: if c.range.1 > c.range.0 {
                                Span::new(file_id, c.range.0, c.range.1)
                            } else {
                                Span::dummy()
                            },
                            expr: lower_expr(c.expr, &mem_stems, &locals),
                        })
                        .collect();
                    instances.push(IrInstance {
                        module_name: module_name.clone(),
                        parameter_assignments: param_ir,
                        instance_name: format!("{}__{}", instance_stem, k),
                        connections: conns_ir,
                    });
                }
            }
            CstModuleItem::GenerateForAssigns {
                loop_var,
                upper_expr,
                assigns: body_assigns,
            } => {
                use crate::expr_const::const_eval_param_expr;
                use crate::parser::{subst_expr_loop_var, subst_target_loop_var};
                let n = const_eval_param_expr(&upper_expr, &locals)
                    .unwrap_or(0)
                    .max(0) as usize;
                for k in 0..n {
                    for (target, expr, range) in body_assigns.iter().cloned() {
                        let target_k = subst_target_loop_var(target, &loop_var, k as i64);
                        let expr_k = subst_expr_loop_var(expr, &loop_var, k as i64);
                        if let Some(a) = lower_continuous_assign(
                            target_k,
                            expr_k,
                            &mem_stems,
                            &locals,
                            &net_widths,
                            file_id,
                            range,
                        ) {
                            assigns.push(a);
                        }
                    }
                }
            }
            CstModuleItem::GenerateForBody { .. }
            | CstModuleItem::GenerateIf { .. }
            | CstModuleItem::GenerateCase { .. } => {
                elaborate_generate_item(
                    item,
                    &mem_stems,
                    &locals,
                    &net_widths,
                    file_id,
                    &mut assigns,
                    &mut instances,
                );
            }
            CstModuleItem::Always { sensitivity, body } => {
                always_blocks.push(lower_always(
                    sensitivity,
                    body,
                    &mem_stems,
                    &locals,
                    &net_widths,
                    file_id,
                ));
            }
            CstModuleItem::Initial { body } => {
                initial_blocks.push(IrInitial {
                    stmts: lower_stmt_block(body, &mem_stems, &locals, &net_widths, file_id),
                });
            }
            CstModuleItem::LocalParam { .. } => {}
        }
    }
    IrModule {
        name: cst.name,
        path: cst.path,
        file_id,
        ports: cst.ports,
        nets,
        assigns,
        instances,
        always_blocks,
        initial_blocks,
        mem_arrays,
        resolved_parameters: locals,
    }
}

fn lower_always(
    sens: Sensitivity,
    body: CstBlock,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
    file_id: FileId,
) -> IrAlways {
    let sensitivity = match sens {
        Sensitivity::Star => IrSensitivity::Star,
        Sensitivity::EdgeList(edges) => IrSensitivity::EdgeList(
            edges
                .into_iter()
                .map(|e| IrSensEntry {
                    edge: match e.edge {
                        EdgeKind::Posedge => IrEdgeKind::Posedge,
                        EdgeKind::Negedge => IrEdgeKind::Negedge,
                        EdgeKind::Level => IrEdgeKind::Level,
                    },
                    signal: e.signal,
                })
                .collect(),
        ),
    };
    IrAlways {
        sensitivity,
        stmts: lower_stmt_block(body, mem_stems, locals, net_widths, file_id),
    }
}

/// Lower a [`CstBlock`] into a [`StmtBlock`] with spans, filtering out statements
/// that fail to lower (e.g. unsupported syntax).
fn lower_stmt_block(
    body: CstBlock,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
    file_id: FileId,
) -> StmtBlock {
    let (stmts, ranges) = body.into_parts();
    let mut out = StmtBlock::with_capacity(stmts.len());
    for (s, (start, end)) in stmts.into_iter().zip(ranges.into_iter()) {
        let span = Span::new(file_id, start, end);
        // Block (from an inlined multi-statement task body): recursively
        // lower the inner block and splice each resulting statement into
        // this stream so the caller's IR is flat.
        if let CstStmt::Block(inner) = s {
            let lowered = lower_stmt_block(inner, mem_stems, locals, net_widths, file_id);
            for (ir, sp) in lowered.iter_with_spans() {
                out.push(ir.clone(), sp);
            }
            continue;
        }
        // Concat-LHS (`{a, b, c} = rhs;`) lowers to multiple per-component
        // assignments; expand inline so each lands as its own statement
        // sharing the source span of the original concat.
        let concat_kind = match &s {
            CstStmt::BlockingAssign { target: AssignTarget::Concat(_), .. } => Some(false),
            CstStmt::NonBlockingAssign { target: AssignTarget::Concat(_), .. } => Some(true),
            _ => None,
        };
        if let Some(is_nb) = concat_kind {
            let (target, rhs) = match s {
                CstStmt::BlockingAssign { target, rhs } => (target, rhs),
                CstStmt::NonBlockingAssign { target, rhs } => (target, rhs),
                _ => unreachable!(),
            };
            for ir in lower_concat_assign_stmts(target, rhs, is_nb, mem_stems, locals, net_widths) {
                out.push(ir, span);
            }
            continue;
        }
        if let Some(ir) = lower_stmt(s, mem_stems, locals, net_widths, file_id) {
            out.push(ir, span);
        }
    }
    out
}

fn net_width_or_default(net_widths: &HashMap<String, usize>, reg: &str) -> usize {
    net_widths.get(reg).copied().unwrap_or(32).clamp(1, 62)
}

/// Packed `reg[i] = val` as read–modify–write on the whole vector (IEEE 1364).
fn lower_packed_bit_assign(lhs: String, idx_ir: IrExpr, val_ir: IrExpr, width: usize) -> IrStmt {
    let w_i = width.clamp(1, 62) as i64;
    let width_mask = IrExpr::Const((1i64 << w_i) - 1);
    let old = IrExpr::Ident(lhs.clone());
    let one = IrExpr::Const(1);
    let shl_at = IrExpr::Binary {
        op: IrBinOp::Shl,
        left: Box::new(one),
        right: Box::new(idx_ir.clone()),
    };
    let at_mask = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(shl_at),
        right: Box::new(width_mask.clone()),
    };
    let not_m = IrExpr::Unary {
        op: IrUnaryOp::Not,
        operand: Box::new(at_mask.clone()),
    };
    let not_masked = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(not_m),
        right: Box::new(width_mask.clone()),
    };
    let cleared = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(old),
        right: Box::new(not_masked),
    };
    let v1 = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(val_ir),
        right: Box::new(IrExpr::Const(1)),
    };
    let shifted = IrExpr::Binary {
        op: IrBinOp::Shl,
        left: Box::new(v1),
        right: Box::new(idx_ir),
    };
    let rhs = IrExpr::Binary {
        op: IrBinOp::Or,
        left: Box::new(cleared),
        right: Box::new(shifted),
    };
    IrStmt::BlockingAssign { lhs, rhs }
}

/// Constant `reg[msb:lsb] = val` (indices known at compile time).
fn lower_packed_part_assign_const(
    lhs: String,
    msb: i64,
    lsb: i64,
    val_ir: IrExpr,
    width: usize,
) -> IrStmt {
    let lo = msb.min(lsb);
    let hi = msb.max(lsb);
    let nbits = hi - lo + 1;
    if nbits <= 0 || nbits > 62 {
        return IrStmt::BlockingAssign { lhs, rhs: val_ir };
    }
    let w_i = width.clamp(1, 62) as i64;
    let width_mask = IrExpr::Const((1i64 << w_i) - 1);
    let field_mask = (((1i64 << nbits) - 1) << lo) & ((1i64 << w_i) - 1);
    let field_mask_ir = IrExpr::Const(field_mask);
    let old = IrExpr::Ident(lhs.clone());
    let not_field = IrExpr::Unary {
        op: IrUnaryOp::Not,
        operand: Box::new(field_mask_ir.clone()),
    };
    let not_masked = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(not_field),
        right: Box::new(width_mask.clone()),
    };
    let cleared = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(old),
        right: Box::new(not_masked),
    };
    let slice_mask = IrExpr::Const((1i64 << nbits) - 1);
    let val_clamped = IrExpr::Binary {
        op: IrBinOp::And,
        left: Box::new(val_ir),
        right: Box::new(slice_mask),
    };
    let shifted = IrExpr::Binary {
        op: IrBinOp::Shl,
        left: Box::new(val_clamped),
        right: Box::new(IrExpr::Const(lo)),
    };
    let rhs = IrExpr::Binary {
        op: IrBinOp::Or,
        left: Box::new(cleared),
        right: Box::new(shifted),
    };
    IrStmt::BlockingAssign { lhs, rhs }
}

/// Continuous `assign` → [`IrAssign`] (packed bit/part selects use the same R/M/W lowering as procedural assigns).
fn lower_continuous_assign(
    target: AssignTarget,
    rhs: Expr,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
    file_id: FileId,
    range: (u32, u32),
) -> Option<IrAssign> {
    let stmt = lower_assign_from_target(target, rhs, false, mem_stems, locals, net_widths)?;
    let span = Span::new(file_id, range.0, range.1);
    match stmt {
        IrStmt::BlockingAssign { lhs, rhs } => Some(IrAssign { lhs, rhs, span }),
        IrStmt::NonBlockingAssign { .. } => None,
        _ => None,
    }
}

fn lower_assign_from_target(
    target: AssignTarget,
    rhs: Expr,
    is_nb: bool,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
) -> Option<IrStmt> {
    // Concat-LHS is multi-statement; `lower_stmt_block` intercepts it first.
    // Continuous-assign callers that hit this path silently drop the
    // statement, which matches the prior behaviour for unsupported forms.
    if matches!(target, AssignTarget::Concat(_)) {
        return None;
    }
    fn wrap_nb(stmt: IrStmt, is_nb: bool) -> IrStmt {
        if is_nb {
            if let IrStmt::BlockingAssign { lhs, rhs } = stmt {
                return IrStmt::NonBlockingAssign { lhs, rhs };
            }
        }
        stmt
    }
    Some(match target {
        AssignTarget::Whole(name) => {
            let rhs_ir = lower_expr(rhs, mem_stems, locals);
            if is_nb {
                IrStmt::NonBlockingAssign { lhs: name, rhs: rhs_ir }
            } else {
                IrStmt::BlockingAssign { lhs: name, rhs: rhs_ir }
            }
        }
        AssignTarget::BitSelect { reg, index } => {
            if mem_stems.contains(&reg) {
                let idx_ir = lower_expr(index, mem_stems, locals);
                let rhs_ir = lower_expr(rhs, mem_stems, locals);
                if let IrExpr::Const(k) = &idx_ir {
                    let lhs = format!("{}__{}", reg, k);
                    return Some(wrap_nb(
                        IrStmt::BlockingAssign { lhs, rhs: rhs_ir },
                        is_nb,
                    ));
                }
                return Some(IrStmt::MemAssign {
                    stem: reg,
                    index: idx_ir,
                    rhs: rhs_ir,
                    nonblocking: is_nb,
                });
            }
            let w = net_width_or_default(net_widths, &reg);
            wrap_nb(
                lower_packed_bit_assign(
                    reg,
                    lower_expr(index, mem_stems, locals),
                    lower_expr(rhs, mem_stems, locals),
                    w,
                ),
                is_nb,
            )
        }
        AssignTarget::PartSelect { reg, msb, lsb } => {
            let msb_ir = lower_expr(msb, mem_stems, locals);
            let lsb_ir = lower_expr(lsb, mem_stems, locals);
            let val_ir = lower_expr(rhs, mem_stems, locals);
            let w = net_width_or_default(net_widths, &reg);
            let stmt = if let (IrExpr::Const(a), IrExpr::Const(b)) = (&msb_ir, &lsb_ir) {
                lower_packed_part_assign_const(reg, *a, *b, val_ir, w)
            } else {
                IrStmt::BlockingAssign {
                    lhs: reg,
                    rhs: val_ir,
                }
            };
            wrap_nb(stmt, is_nb)
        }
        // Guarded by the early-return at the top of this function.
        AssignTarget::Concat(_) => unreachable!("concat-LHS is handled in lower_stmt_block"),
    })
}

/// Elaborate a generate-body item (GenerateForBody / GenerateIf / GenerateCase)
/// or any normal item that may appear in such a body. Recurses through nested
/// generate constructs. Loop variables are substituted at each iteration with
/// [`subst_module_item_loop_var`].
fn elaborate_generate_item(
    item: CstModuleItem,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
    file_id: FileId,
    assigns: &mut Vec<IrAssign>,
    instances: &mut Vec<IrInstance>,
) {
    use crate::expr_const::const_eval_param_expr;
    use crate::parser::{
        subst_expr_loop_var, subst_module_item_loop_var, subst_port_connections,
        subst_target_loop_var,
    };
    match item {
        CstModuleItem::Assign { target, expr, range } => {
            if let Some(a) = lower_continuous_assign(
                target, expr, mem_stems, locals, net_widths, file_id, range,
            ) {
                assigns.push(a);
            }
        }
        CstModuleItem::Instance {
            module_name,
            parameter_assignments,
            instance_name,
            connections,
        } => {
            let param_ir: Vec<(String, IrExpr)> = parameter_assignments
                .into_iter()
                .map(|(n, e)| (n, lower_expr(e, mem_stems, locals)))
                .collect();
            let conns = connections
                .into_iter()
                .map(|c| IrPortConn {
                    port_name: c.port_name,
                    span: if c.range.1 > c.range.0 {
                        Span::new(file_id, c.range.0, c.range.1)
                    } else {
                        Span::dummy()
                    },
                    expr: lower_expr(c.expr, mem_stems, locals),
                })
                .collect();
            instances.push(IrInstance {
                module_name,
                parameter_assignments: param_ir,
                instance_name,
                connections: conns,
            });
        }
        CstModuleItem::GenerateFor {
            loop_var,
            upper_expr,
            module_name,
            parameter_assignments,
            instance_stem,
            connections,
        } => {
            let n = const_eval_param_expr(&upper_expr, locals)
                .unwrap_or(0)
                .max(0) as usize;
            for k in 0..n {
                let conns = subst_port_connections(&connections, &loop_var, k as i64);
                let param_ir: Vec<(String, IrExpr)> = parameter_assignments
                    .iter()
                    .cloned()
                    .map(|(n, e)| (n, lower_expr(e, mem_stems, locals)))
                    .collect();
                let conns_ir = conns
                    .into_iter()
                    .map(|c| IrPortConn {
                        port_name: c.port_name,
                        span: if c.range.1 > c.range.0 {
                            Span::new(file_id, c.range.0, c.range.1)
                        } else {
                            Span::dummy()
                        },
                        expr: lower_expr(c.expr, mem_stems, locals),
                    })
                    .collect();
                instances.push(IrInstance {
                    module_name: module_name.clone(),
                    parameter_assignments: param_ir,
                    instance_name: format!("{}__{}", instance_stem, k),
                    connections: conns_ir,
                });
            }
        }
        CstModuleItem::GenerateForAssigns {
            loop_var,
            upper_expr,
            assigns: body_assigns,
        } => {
            let n = const_eval_param_expr(&upper_expr, locals)
                .unwrap_or(0)
                .max(0) as usize;
            for k in 0..n {
                for (target, expr, range) in body_assigns.iter().cloned() {
                    let target_k = subst_target_loop_var(target, &loop_var, k as i64);
                    let expr_k = subst_expr_loop_var(expr, &loop_var, k as i64);
                    if let Some(a) = lower_continuous_assign(
                        target_k, expr_k, mem_stems, locals, net_widths, file_id, range,
                    ) {
                        assigns.push(a);
                    }
                }
            }
        }
        CstModuleItem::GenerateForBody { loop_var, upper_expr, body } => {
            let n = const_eval_param_expr(&upper_expr, locals)
                .unwrap_or(0)
                .max(0) as usize;
            for k in 0..n {
                for inner in body.iter().cloned() {
                    let substituted = subst_module_item_loop_var(inner, &loop_var, k as i64);
                    elaborate_generate_item(
                        substituted,
                        mem_stems,
                        locals,
                        net_widths,
                        file_id,
                        assigns,
                        instances,
                    );
                }
            }
        }
        CstModuleItem::GenerateIf { cond, then_body, else_body } => {
            let c = const_eval_param_expr(&cond, locals).unwrap_or(0);
            let chosen = if c != 0 { then_body } else { else_body };
            for inner in chosen {
                elaborate_generate_item(
                    inner, mem_stems, locals, net_widths, file_id, assigns, instances,
                );
            }
        }
        CstModuleItem::GenerateCase { scrutinee, arms, default } => {
            let s = const_eval_param_expr(&scrutinee, locals).unwrap_or(0);
            let mut matched: Option<Vec<CstModuleItem>> = None;
            for (val, body) in arms {
                let v = const_eval_param_expr(&val, locals).unwrap_or(0);
                if v == s {
                    matched = Some(body);
                    break;
                }
            }
            let chosen = matched.unwrap_or(default);
            for inner in chosen {
                elaborate_generate_item(
                    inner, mem_stems, locals, net_widths, file_id, assigns, instances,
                );
            }
        }
        // Other module items (NetDecl, Always, Initial, LocalParam) are not
        // expected inside generate bodies in the test corpus. They could be
        // supported by extending this helper if needed.
        _ => {}
    }
}

/// Width of an LHS component (in bits) used when splitting a concat-LHS into
/// per-component assignments. Non-const part-selects fall back to 1 — they're
/// already a degenerate case for the simulator and lowering rejects them
/// downstream when the bit pattern can't be materialised.
fn concat_component_width(
    t: &AssignTarget,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
) -> usize {
    match t {
        AssignTarget::Whole(name) => net_widths.get(name).copied().unwrap_or(1),
        AssignTarget::BitSelect { .. } => 1,
        AssignTarget::PartSelect { msb, lsb, .. } => {
            let m = crate::expr_const::const_eval_param_expr(msb, locals).unwrap_or(0);
            let l = crate::expr_const::const_eval_param_expr(lsb, locals).unwrap_or(0);
            ((m - l).unsigned_abs() as usize) + 1
        }
        AssignTarget::Concat(parts) => parts
            .iter()
            .map(|p| concat_component_width(p, locals, net_widths))
            .sum(),
    }
}

/// Lower a concat-LHS assignment `{a, b, c} = rhs` into a sequence of
/// per-component blocking (or non-blocking) assignments. Components are
/// MSB-first in source, so the leftmost element receives the high bits.
fn lower_concat_assign_stmts(
    target: AssignTarget,
    rhs: Expr,
    is_nb: bool,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
) -> Vec<IrStmt> {
    let comps = match target {
        AssignTarget::Concat(c) => c,
        _ => return vec![],
    };
    let rhs_ir = lower_expr(rhs, mem_stems, locals);
    expand_concat_with_rhs_ir(comps, rhs_ir, is_nb, mem_stems, locals, net_widths)
}

fn expand_concat_with_rhs_ir(
    comps: Vec<AssignTarget>,
    rhs_ir: IrExpr,
    is_nb: bool,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
) -> Vec<IrStmt> {
    fn wrap_nb(stmt: IrStmt, is_nb: bool) -> IrStmt {
        if is_nb {
            if let IrStmt::BlockingAssign { lhs, rhs } = stmt {
                return IrStmt::NonBlockingAssign { lhs, rhs };
            }
        }
        stmt
    }
    let widths: Vec<usize> = comps
        .iter()
        .map(|c| concat_component_width(c, locals, net_widths))
        .collect();
    let total: usize = widths.iter().sum();
    let mut out = Vec::new();
    let mut shift = total;
    for (comp, w) in comps.into_iter().zip(widths.into_iter()) {
        // First component (MSB) gets the high slice; track shift downward.
        shift = shift.saturating_sub(w);
        let mask: i64 = if w >= 63 { !0 } else { (1i64 << w) - 1 };
        let shifted = if shift == 0 {
            rhs_ir.clone()
        } else {
            IrExpr::Binary {
                op: IrBinOp::Shr,
                left: Box::new(rhs_ir.clone()),
                right: Box::new(IrExpr::Const(shift as i64)),
            }
        };
        let masked = IrExpr::Binary {
            op: IrBinOp::And,
            left: Box::new(shifted),
            right: Box::new(IrExpr::Const(mask)),
        };
        match comp {
            AssignTarget::Concat(inner) => {
                out.extend(expand_concat_with_rhs_ir(
                    inner, masked, is_nb, mem_stems, locals, net_widths,
                ));
            }
            AssignTarget::Whole(name) => {
                let stmt = IrStmt::BlockingAssign {
                    lhs: name,
                    rhs: masked,
                };
                out.push(wrap_nb(stmt, is_nb));
            }
            AssignTarget::BitSelect { reg, index } => {
                let idx_ir = lower_expr(index, mem_stems, locals);
                let w_outer = net_width_or_default(net_widths, &reg);
                let stmt = lower_packed_bit_assign(reg, idx_ir, masked, w_outer);
                out.push(wrap_nb(stmt, is_nb));
            }
            AssignTarget::PartSelect { reg, msb, lsb } => {
                let msb_ir = lower_expr(msb, mem_stems, locals);
                let lsb_ir = lower_expr(lsb, mem_stems, locals);
                let w_outer = net_width_or_default(net_widths, &reg);
                let stmt = if let (IrExpr::Const(a), IrExpr::Const(b)) = (&msb_ir, &lsb_ir) {
                    lower_packed_part_assign_const(reg, *a, *b, masked, w_outer)
                } else {
                    IrStmt::BlockingAssign {
                        lhs: reg,
                        rhs: masked,
                    }
                };
                out.push(wrap_nb(stmt, is_nb));
            }
        }
    }
    out
}

fn lower_stmt(
    s: CstStmt,
    mem_stems: &HashSet<String>,
    locals: &HashMap<String, i64>,
    net_widths: &HashMap<String, usize>,
    file_id: FileId,
) -> Option<IrStmt> {
    match s {
        CstStmt::BlockingAssign { target, rhs } => {
            lower_assign_from_target(target, rhs, false, mem_stems, locals, net_widths)
        }
        CstStmt::NonBlockingAssign { target, rhs } => {
            lower_assign_from_target(target, rhs, true, mem_stems, locals, net_widths)
        }
        CstStmt::IfElse {
            cond,
            then_body,
            else_body,
        } => Some(IrStmt::IfElse {
            cond: lower_expr(cond, mem_stems, locals),
            then_body: lower_stmt_block(then_body, mem_stems, locals, net_widths, file_id),
            else_body: lower_stmt_block(else_body, mem_stems, locals, net_widths, file_id),
        }),
        CstStmt::Case {
            kind,
            expr,
            arms,
            default,
        } => Some(IrStmt::Case {
            expr: lower_expr(expr, mem_stems, locals),
            arms: arms
                .into_iter()
                .map(|a| {
                    let care_mask = if kind == crate::parser::CaseKind::Plain {
                        None
                    } else {
                        wildcard_mask_from_expr(&a.value)
                    };
                    IrCaseArm {
                        value: lower_expr(a.value, mem_stems, locals),
                        body: lower_stmt_block(a.body, mem_stems, locals, net_widths, file_id),
                        care_mask,
                    }
                })
                .collect(),
            default: lower_stmt_block(default, mem_stems, locals, net_widths, file_id),
        }),
        CstStmt::For {
            init_var,
            init_val,
            cond,
            step_var,
            step_expr,
            body,
        } => Some(IrStmt::For {
            init_var,
            init_val: lower_expr(init_val, mem_stems, locals),
            cond: lower_expr(cond, mem_stems, locals),
            step_var,
            step_expr: lower_expr(step_expr, mem_stems, locals),
            body: lower_stmt_block(body, mem_stems, locals, net_widths, file_id),
        }),
        CstStmt::Delay(d) => Some(IrStmt::Delay(d)),
        CstStmt::SystemTask { name, args } => Some(IrStmt::SystemTask {
            name,
            args: args
                .into_iter()
                .map(|e| lower_expr(e, mem_stems, locals))
                .collect(),
        }),
        // Block is spliced by `lower_stmt_block` before reaching here.
        CstStmt::Block(_) => None,
    }
}

fn lower_expr(e: Expr, mem_stems: &HashSet<String>, locals: &HashMap<String, i64>) -> IrExpr {
    match e {
        Expr::Ident(name) => {
            if let Some(&v) = locals.get(&name) {
                IrExpr::Const(v)
            } else {
                IrExpr::Ident(name)
            }
        }
        Expr::Number(lit) => IrExpr::Const(crate::expr_const::parse_verilog_number(&lit)),
        Expr::Index { base, msb, lsb } => {
            match lsb {
                None => {
                    if let Expr::Ident(stem) = &*base {
                        if mem_stems.contains(stem) {
                            IrExpr::MemRead {
                                stem: stem.clone(),
                                index: Box::new(lower_expr(*msb, mem_stems, locals)),
                            }
                        } else {
                            let base_ir = lower_expr(*base, mem_stems, locals);
                            let m = lower_expr(*msb, mem_stems, locals);
                            IrExpr::PartSelect {
                                value: Box::new(base_ir),
                                msb: Box::new(m.clone()),
                                lsb: Box::new(m),
                            }
                        }
                    } else {
                        let base_ir = lower_expr(*base, mem_stems, locals);
                        let m = lower_expr(*msb, mem_stems, locals);
                        IrExpr::PartSelect {
                            value: Box::new(base_ir),
                            msb: Box::new(m.clone()),
                            lsb: Box::new(m),
                        }
                    }
                }
                Some(lsb_e) => IrExpr::PartSelect {
                    value: Box::new(lower_expr(*base, mem_stems, locals)),
                    msb: Box::new(lower_expr(*msb, mem_stems, locals)),
                    lsb: Box::new(lower_expr(*lsb_e, mem_stems, locals)),
                },
            }
        }
        Expr::Binary { op, left, right } => IrExpr::Binary {
            op: lower_binop(op),
            left: Box::new(lower_expr(*left, mem_stems, locals)),
            right: Box::new(lower_expr(*right, mem_stems, locals)),
        },
        Expr::Unary { op, operand } => {
            if op == UnaryOp::Pos {
                // +x is identity — drop the unary
                return lower_expr(*operand, mem_stems, locals);
            }
            IrExpr::Unary {
                op: lower_unaryop(op),
                operand: Box::new(lower_expr(*operand, mem_stems, locals)),
            }
        }
        Expr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => IrExpr::Ternary {
            cond: Box::new(lower_expr(*cond, mem_stems, locals)),
            then_expr: Box::new(lower_expr(*then_expr, mem_stems, locals)),
            else_expr: Box::new(lower_expr(*else_expr, mem_stems, locals)),
        },
        Expr::Concat(exprs) => {
            IrExpr::Concat(exprs.into_iter().map(|e| lower_expr(e, mem_stems, locals)).collect())
        }
        Expr::Clog2(arg) => {
            let v_inner =
                crate::expr_const::const_eval_param_expr(arg.as_ref(), locals).unwrap_or(0);
            IrExpr::Const(crate::expr_const::verilog_clog2(v_inner))
        }
        Expr::Signed(arg) => IrExpr::Signed(Box::new(lower_expr(*arg, mem_stems, locals))),
    }
}

fn lower_binop(op: BinaryOp) -> IrBinOp {
    match op {
        BinaryOp::Add => IrBinOp::Add,
        BinaryOp::Sub => IrBinOp::Sub,
        BinaryOp::Mul => IrBinOp::Mul,
        BinaryOp::Div => IrBinOp::Div,
        BinaryOp::Mod => IrBinOp::Mod,
        BinaryOp::And => IrBinOp::And,
        BinaryOp::Or => IrBinOp::Or,
        BinaryOp::Xor => IrBinOp::Xor,
        BinaryOp::Shl => IrBinOp::Shl,
        BinaryOp::Shr => IrBinOp::Shr,
        BinaryOp::Ashr => IrBinOp::Ashr,
        BinaryOp::LogAnd => IrBinOp::LogAnd,
        BinaryOp::LogOr => IrBinOp::LogOr,
        BinaryOp::Eq => IrBinOp::Eq,
        BinaryOp::Ne => IrBinOp::Ne,
        BinaryOp::Lt => IrBinOp::Lt,
        BinaryOp::Le => IrBinOp::Le,
        BinaryOp::Gt => IrBinOp::Gt,
        BinaryOp::Ge => IrBinOp::Ge,
    }
}

fn lower_unaryop(op: UnaryOp) -> IrUnaryOp {
    match op {
        UnaryOp::Not => IrUnaryOp::Not,
        UnaryOp::LogNot => IrUnaryOp::LogNot,
        UnaryOp::Neg => IrUnaryOp::Neg,
        UnaryOp::Pos => unreachable!("Pos handled before calling lower_unaryop"),
    }
}

// ── Helper: recursive directory walk ─────────────────────────────────

fn walk_dir<F>(root: &Path, f: &mut F) -> std::io::Result<()>
where
    F: FnMut(&Path),
{
    if root.is_file() {
        if is_verilog_file(root) {
            f(root);
        }
        return Ok(());
    }

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                "target" | "node_modules" | ".git" | "dist" | "tests" | "fixtures"
            ) {
                continue;
            }
            walk_dir(&path, f)?;
        } else if is_verilog_file(&path) {
            f(&path);
        }
    }
    Ok(())
}

fn is_verilog_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_lowercase().as_str(), "v" | "sv"),
        None => false,
    }
}

/// Sum of every `#delay` literal reachable in `initial` bodies for modules defined in `source_file`
/// ([`IrModule::path`] compared with `source_file`). Branches (`if`/`case`) are all included.
pub fn sum_initial_delay_literals_for_source_file(project: &IrProject, source_file: &Path) -> usize {
    let mut total = DelayRational::ZERO;
    for m in &project.modules {
        if !module_path_matches_source_file(&m.path, source_file) {
            continue;
        }
        for ib in &m.initial_blocks {
            total = total.add(sum_delay_literals_in_stmts(&ib.stmts));
        }
    }
    total.ceil_whole_time_units()
}

fn module_path_matches_source_file(module_path: &str, source_file: &Path) -> bool {
    let mp = Path::new(module_path);
    if mp == source_file {
        return true;
    }
    if let (Ok(a), Ok(b)) = (mp.canonicalize(), source_file.canonicalize()) {
        return a == b;
    }
    module_path == source_file.to_string_lossy().as_ref()
}

/// Constant trip count for `for (v=…; v op N; v=v+k)` (matches optimizer `try_unroll` rules).
pub(crate) fn static_for_iteration_count(
    init_var: &str,
    init_val: &IrExpr,
    cond: &IrExpr,
    step_var: &str,
    step_expr: &IrExpr,
) -> Option<usize> {
    if init_var != step_var {
        return None;
    }
    let start = match init_val {
        IrExpr::Const(v) => *v,
        _ => return None,
    };
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
    if iterations <= 0 || iterations > 10_000_000 {
        return None;
    }
    Some(iterations as usize)
}

fn sum_delay_literals_in_stmts(stmts: &[IrStmt]) -> DelayRational {
    let mut s = DelayRational::ZERO;
    for st in stmts {
        match st {
            IrStmt::Delay(d) => s = s.add(*d),
            IrStmt::IfElse {
                then_body,
                else_body,
                ..
            } => {
                s = s.add(sum_delay_literals_in_stmts(then_body));
                s = s.add(sum_delay_literals_in_stmts(else_body));
            }
            IrStmt::Case {
                arms, default, ..
            } => {
                for a in arms {
                    s = s.add(sum_delay_literals_in_stmts(&a.body));
                }
                s = s.add(sum_delay_literals_in_stmts(default));
            }
            IrStmt::For {
                init_var,
                init_val,
                cond,
                step_var,
                step_expr,
                body,
            } => {
                if let Some(n) =
                    static_for_iteration_count(init_var, init_val, cond, step_var, step_expr)
                {
                    let per = sum_delay_literals_in_stmts(body);
                    s = s.add(per.saturating_mul_u128(n as u128));
                } else {
                    s = s.add(sum_delay_literals_in_stmts(body));
                }
            }
            IrStmt::BlockingAssign { .. }
            | IrStmt::NonBlockingAssign { .. }
            | IrStmt::MemAssign { .. }
            |             IrStmt::SystemTask { .. } => {}
        }
    }
    s
}

#[cfg(test)]
mod ir_try_eval_const_index_tests {
    use super::*;

    #[test]
    fn const_index_fold_adds_literals() {
        let e = IrExpr::Binary {
            op: IrBinOp::Add,
            left: Box::new(IrExpr::Const(2)),
            right: Box::new(IrExpr::Const(1)),
        };
        assert_eq!(ir_try_eval_const_index_expr(&e), Some(3));
    }

    #[test]
    fn const_index_rejects_signal_ident() {
        assert_eq!(ir_try_eval_const_index_expr(&IrExpr::Ident("i".into())), None);
    }

    #[test]
    fn const_index_nested_add_matches_generate_i_plus_one_shape() {
        // After generate unrolling, `c[i+1]` often becomes `c[k+1]` with both as constants in IR.
        let e = IrExpr::Binary {
            op: IrBinOp::Add,
            left: Box::new(IrExpr::Const(7)),
            right: Box::new(IrExpr::Const(1)),
        };
        assert_eq!(ir_try_eval_const_index_expr(&e), Some(8));
    }
}


/// Parse a literal source string like `8'b1???_????` or `4'hF?` and return
/// `(value, mask)` where `mask` has 1s only in bit positions whose value is
/// known (not `?`/`x`/`z`). Returns `None` if the expression isn't a
/// recognizable sized/unsized binary or hex literal — caller falls back to
/// exact-equality matching.
fn wildcard_mask_from_expr(e: &crate::parser::Expr) -> Option<i64> {
    let s = match e {
        crate::parser::Expr::Number(n) => n.as_str(),
        _ => return None,
    };
    let apos = s.find('\'')?;
    let rest = &s[apos + 1..];
    let base = rest.chars().next()?;
    let digits = &rest[1..];
    let (_value, mask) = match base {
        'b' | 'B' => parse_binary_literal_with_mask(digits)?,
        'h' | 'H' => parse_hex_literal_with_mask(digits)?,
        _ => return None,
    };
    // Only flag wildcards when the mask actually has 0s in care positions.
    // For a plain `8'b1010` mask=0xFF — equivalent to exact-equality, so we
    // can return None and skip the masking overhead.
    let nbits = (mask.checked_ilog2().unwrap_or(0) + 1) as usize;
    let full_mask: i64 = if nbits >= 63 { !0 } else { (1i64 << nbits) - 1 };
    if mask == full_mask {
        None
    } else {
        Some(mask)
    }
}

fn parse_binary_literal_with_mask(digits: &str) -> Option<(i64, i64)> {
    let mut value: i64 = 0;
    let mut mask: i64 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        value <<= 1;
        mask <<= 1;
        match c {
            '0' => {
                mask |= 1;
            }
            '1' => {
                value |= 1;
                mask |= 1;
            }
            '?' | 'x' | 'X' | 'z' | 'Z' => { /* care bit stays 0 */ }
            _ => return None,
        }
    }
    Some((value, mask))
}

fn parse_hex_literal_with_mask(digits: &str) -> Option<(i64, i64)> {
    let mut value: i64 = 0;
    let mut mask: i64 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        value <<= 4;
        mask <<= 4;
        match c {
            '0'..='9' | 'a'..='f' | 'A'..='F' => {
                value |= c.to_digit(16)? as i64;
                mask |= 0xF;
            }
            '?' | 'x' | 'X' | 'z' | 'Z' => { /* care nibble stays 0 */ }
            _ => return None,
        }
    }
    Some((value, mask))
}
