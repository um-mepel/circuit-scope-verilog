use crate::delay_rational::DelayRational;
use crate::lexer::{Token, TokenKind};
use crate::{Diagnostic, Module, ParseResult, Port, Severity, SourceFile};

/// A list of [`CstStmt`]s carrying parallel byte-range info for each statement.
///
/// Looks like a `Vec<CstStmt>` to most consumers thanks to `Deref<Target=[CstStmt]>`;
/// span info is accessed via [`CstBlock::ranges`] / [`CstBlock::range_of`].
#[derive(Debug, Clone, Default)]
pub struct CstBlock {
    stmts: Vec<CstStmt>,
    /// Half-open byte ranges `(start, end)` per statement in the source file.
    ranges: Vec<(u32, u32)>,
}

impl CstBlock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, stmt: CstStmt, range: (u32, u32)) {
        self.stmts.push(stmt);
        self.ranges.push(range);
    }

    pub fn stmts(&self) -> &[CstStmt] {
        &self.stmts
    }

    pub fn ranges(&self) -> &[(u32, u32)] {
        &self.ranges
    }

    pub fn range_of(&self, idx: usize) -> Option<(u32, u32)> {
        self.ranges.get(idx).copied()
    }

    pub fn len(&self) -> usize {
        self.stmts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stmts.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, CstStmt> {
        self.stmts.iter()
    }

    pub fn iter_with_ranges(&self) -> impl Iterator<Item = (&CstStmt, (u32, u32))> + '_ {
        self.stmts.iter().zip(self.ranges.iter().copied())
    }

    pub fn into_parts(self) -> (Vec<CstStmt>, Vec<(u32, u32)>) {
        (self.stmts, self.ranges)
    }
}

impl std::ops::Deref for CstBlock {
    type Target = [CstStmt];
    fn deref(&self) -> &[CstStmt] {
        &self.stmts
    }
}

impl<'a> IntoIterator for &'a CstBlock {
    type Item = &'a CstStmt;
    type IntoIter = std::slice::Iter<'a, CstStmt>;
    fn into_iter(self) -> Self::IntoIter {
        self.stmts.iter()
    }
}

impl FromIterator<(CstStmt, (u32, u32))> for CstBlock {
    fn from_iter<I: IntoIterator<Item = (CstStmt, (u32, u32))>>(iter: I) -> Self {
        let mut b = CstBlock::new();
        for (s, r) in iter {
            b.push(s, r);
        }
        b
    }
}

/// Concrete syntax for a parsed Verilog (IEEE 1364) file. Intentionally minimal: records modules
/// and body items for lowering to IR — not a full SystemVerilog front end.
#[derive(Debug, Clone)]
pub struct CstFile {
    pub modules: Vec<CstModule>,
}

/// Concrete syntax node for a `module` declaration.
#[derive(Debug, Clone)]
pub struct CstModule {
    pub name: String,
    pub ports: Vec<Port>,
    /// `#(parameter W = 16, ...)` from the module header.
    pub module_parameters: Vec<(String, Expr)>,
    pub path: String,
    pub items: Vec<CstModuleItem>,
}

/// Module body item.
#[derive(Debug, Clone)]
pub enum CstModuleItem {
    NetDecl {
        kind: NetKind,
        /// Packed `[msb:lsb]` when present; scalar width uses `width` when this is `None`.
        packed_dim: Option<(Expr, Expr)>,
        width: usize,
        names: Vec<String>,
        /// Unpacked `[hi:lo]` per stem (`x[0:9]`); bounds may be expressions (e.g. `[BoothIter-1:0]`).
        unpacked_stems: Vec<(String, Expr, Expr)>,
        /// `input` / `output` / `inout` from a directional port declaration in the module body.
        decl_dir: Option<String>,
    },
    Assign {
        target: AssignTarget,
        expr: Expr,
        /// Byte range of the `assign … ;` statement (inclusive of the trailing
        /// semicolon) for driver-trace reporting. `(0, 0)` when unavailable.
        range: (u32, u32),
    },
    Instance {
        module_name: String,
        /// `#(.param(expr), …)` — empty if no parameter override list.
        parameter_assignments: Vec<(String, Expr)>,
        instance_name: String,
        connections: Vec<PortConnection>,
    },
    Always {
        sensitivity: Sensitivity,
        body: CstBlock,
    },
    Initial {
        body: CstBlock,
    },
    /// `localparam` / `parameter` assignments (`localparam a = 1, b = 2;`).
    LocalParam {
        assignments: Vec<(String, Expr)>,
    },
    /// `generate for (i=0; i<N; i=i+1) begin … <one instance> end` — expanded during IR lowering
    /// so `N` uses the module's **current** parameters (specialized `#(.W(11))`, etc.).
    GenerateFor {
        loop_var: String,
        upper_expr: Expr,
        module_name: String,
        parameter_assignments: Vec<(String, Expr)>,
        instance_stem: String,
        connections: Vec<PortConnection>,
    },
    /// `generate for (i=0; i<N; i=i+1) begin assign y[i] = …; end` —
    /// expanded during IR lowering to N continuous assigns with the loop
    /// variable substituted. Mirrors [`GenerateFor`] but the body is a list
    /// of `assign` statements instead of a single instance.
    GenerateForAssigns {
        loop_var: String,
        upper_expr: Expr,
        assigns: Vec<(AssignTarget, Expr, (u32, u32))>,
    },
    /// Most general `generate for` form: body is an arbitrary list of module
    /// items including nested generate constructs. Used when the body is
    /// neither "single instance" nor "all continuous assigns". The IR
    /// elaborator recurses through each iteration with the loop variable
    /// substituted.
    GenerateForBody {
        loop_var: String,
        upper_expr: Expr,
        body: Vec<CstModuleItem>,
    },
    /// `generate if (cond) <then> else <else> endgenerate`. The condition is
    /// const-evaluated against module parameters at IR build time and the
    /// chosen branch is elaborated normally.
    GenerateIf {
        cond: Expr,
        then_body: Vec<CstModuleItem>,
        else_body: Vec<CstModuleItem>,
    },
    /// `generate case (...) <arms> [default: ...] endcase endgenerate`. The
    /// scrutinee is const-evaluated and the matching arm (or default) is
    /// elaborated.
    GenerateCase {
        scrutinee: Expr,
        arms: Vec<(Expr, Vec<CstModuleItem>)>,
        default: Vec<CstModuleItem>,
    },
}

/// Port connection: `.port_name(signal_expr)` or **positional** (`expr` only, mapped to child ports by order).
#[derive(Debug, Clone)]
pub struct PortConnection {
    pub port_name: Option<String>,
    pub expr: Expr,
    /// Byte range `[start, end)` spanning the port connection text in the
    /// source file. For named connections we span `.port(expr)` including
    /// both parens; for positional we span just the expression. Used by
    /// the debugger's "Jump to Driver" for synthesized glue assigns (see
    /// `flatten_module`).
    pub range: (u32, u32),
}

/// Sensitivity list for always blocks.
#[derive(Debug, Clone)]
pub enum Sensitivity {
    Star,
    EdgeList(Vec<SensEdge>),
}

#[derive(Debug, Clone)]
pub struct SensEdge {
    pub edge: EdgeKind,
    pub signal: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Posedge,
    Negedge,
    Level,
}

/// Left-hand side of a procedural assignment (`reg`, `reg[i]`, `reg[msb:lsb]`,
/// or a Verilog concat like `{a, b[3:0], c}`). Concat components are stored
/// MSB-first (i.e. the leftmost element in source receives the high bits of
/// the RHS).
#[derive(Debug, Clone)]
pub enum AssignTarget {
    Whole(String),
    BitSelect { reg: String, index: Expr },
    PartSelect {
        reg: String,
        msb: Expr,
        lsb: Expr,
    },
    Concat(Vec<AssignTarget>),
}

/// Procedural statement inside an always/initial block.
#[derive(Debug, Clone)]
pub enum CstStmt {
    BlockingAssign { target: AssignTarget, rhs: Expr },
    NonBlockingAssign { target: AssignTarget, rhs: Expr },
    IfElse {
        cond: Expr,
        then_body: CstBlock,
        else_body: CstBlock,
    },
    Case {
        /// `case` (plain) vs `casez`/`casex` (wildcard-aware). At IR lowering
        /// time, arms whose value is a literal containing `?`/`x`/`z` are
        /// translated to a (value, care_mask) pair for masked equality.
        kind: CaseKind,
        expr: Expr,
        arms: Vec<CaseArm>,
        default: CstBlock,
    },
    For {
        init_var: String,
        init_val: Expr,
        cond: Expr,
        step_var: String,
        step_expr: Expr,
        body: CstBlock,
    },
    Delay(DelayRational),
    SystemTask { name: String, args: Vec<Expr> },
}

/// Distinguishes `case` from `casez`/`casex` so the lowering can choose
/// exact vs wildcard-aware matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseKind {
    Plain,
    Z,
    X,
}

#[derive(Debug, Clone)]
pub struct CaseArm {
    pub value: Expr,
    pub body: CstBlock,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NetKind {
    Wire,
    Reg,
}

/// Expression tree used for assignments and optimisation.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Ident(String),
    Number(String),
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    Concat(Vec<Expr>),
    /// Index or part-select postfix: `base[msb]` or `base[msb:lsb]`.
    Index {
        base: Box<Expr>,
        msb: Box<Expr>,
        lsb: Option<Box<Expr>>,
    },
    /// `$clog2(expr)` — ceiling log2; evaluated for parameters/localparams.
    Clog2(Box<Expr>),
    /// `$signed(expr)` — interpret packed value as signed (IEEE 1364).
    Signed(Box<Expr>),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,      // &
    Or,       // |
    Xor,      // ^
    Shl,      // <<
    Shr,      // >>
    Ashr,     // >>>
    LogAnd,   // &&
    LogOr,    // ||
    Eq,       // ==
    Ne,       // !=
    Lt,       // <
    Le,       // <=
    Gt,       // >
    Ge,       // >=
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UnaryOp {
    Not,    // ~
    LogNot, // !
    Neg,    // -
    Pos,    // +
}

pub(crate) fn parse_cst<'a>(
    file: &'a SourceFile,
    tokens: &'a [Token],
) -> (CstFile, Vec<Diagnostic>) {
    let mut parser = Parser::new(file, tokens);
    parser.parse()
}

pub(crate) fn parse_file(file: &SourceFile, tokens: &[Token]) -> ParseResult {
    let (cst, diagnostics) = parse_cst(file, tokens);

    let modules = cst
        .modules
        .into_iter()
        .map(|m| Module {
            name: m.name,
            ports: m.ports,
            path: m.path,
        })
        .collect();

    ParseResult { modules, diagnostics }
}

/// Module-local function declaration captured for inlining at expression
/// parse time. Only the single-assignment body form
/// `begin name = <expr>; end` is currently supported; richer bodies are
/// rejected at declaration time.
#[derive(Debug, Clone)]
struct FuncDef {
    name: String,
    /// Formal-parameter names, in declaration order.
    args: Vec<String>,
    /// RHS expression of the single `name = <expr>;` body statement.
    body_expr: Expr,
}

/// Module-local task declaration captured for inlining at statement parse
/// time. Only single-statement bodies are currently supported; richer bodies
/// would need block-splicing.
#[derive(Debug, Clone)]
struct TaskDef {
    name: String,
    args: Vec<String>,
    body_stmt: CstStmt,
}

struct Parser<'a> {
    file: &'a SourceFile,
    tokens: &'a [Token],
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    /// Per-module function table; cleared at the start of each module.
    functions: Vec<FuncDef>,
    /// Per-module task table.
    tasks: Vec<TaskDef>,
}

impl<'a> Parser<'a> {
    fn new(file: &'a SourceFile, tokens: &'a [Token]) -> Self {
        Self {
            file,
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            functions: Vec::new(),
            tasks: Vec::new(),
        }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) {
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
    }

    /// End byte offset of the most recently consumed token; 0 if none.
    fn last_end(&self) -> u32 {
        if self.pos == 0 {
            0
        } else {
            self.tokens[self.pos - 1].end as u32
        }
    }

    /// Start byte offset of the next token to be consumed.
    fn cur_start(&self) -> u32 {
        self.current().offset as u32
    }

    fn match_kind(&mut self, kind: TokenKind) -> bool {
        if self.current().kind == kind {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_identifier(&mut self, message: &str) -> Option<String> {
        if self.current().kind == TokenKind::Identifier {
            let name = self.current().lexeme.clone();
            self.bump();
            Some(name)
        } else {
            self.error_at_current(message);
            None
        }
    }

    fn error_at_current(&mut self, message: &str) {
        let tok = self.current();
        let (line, col) = offset_to_line_col(&self.file.content, tok.offset);
        self.diagnostics.push(Diagnostic {
            message: message.to_string(),
            severity: Severity::Error,
            line,
            column: col,
            path: self.file.path.clone(),
        });
        self.bump();
    }

    fn parse(&mut self) -> (CstFile, Vec<Diagnostic>) {
        let mut modules = Vec::new();
        while self.current().kind != TokenKind::Eof {
            if self.current().kind == TokenKind::Module {
                if let Some(m) = self.parse_module() {
                    modules.push(m);
                }
            } else {
                self.bump();
            }
        }
        (
            CstFile { modules },
            std::mem::take(&mut self.diagnostics),
        )
    }

    fn parse_module(&mut self) -> Option<CstModule> {
        self.bump(); // consume 'module'
        let name = self.expect_identifier("expected module name")?;
        let mut ports = Vec::new();

        let module_parameters = self.parse_module_parameter_list();

        if self.match_kind(TokenKind::LParen) {
            // ANSI lists: `output [6:0] a, b` repeats direction and vector width for comma-separated names.
            // Parametric ranges like `[W-1:0]` must be parsed as expressions and evaluated (see eval).
            let mut last_port_direction: Option<String> = None;
            let mut last_port_width: usize = 1;
            'ansi_ports: loop {
                if matches!(self.current().kind, TokenKind::RParen | TokenKind::Eof) {
                    break 'ansi_ports;
                }

                let direction = if matches!(
                    self.current().kind,
                    TokenKind::Input | TokenKind::Output | TokenKind::Inout
                ) {
                    let d = self.current().lexeme.clone();
                    self.bump();
                    last_port_direction = Some(d.clone());
                    // Optional net type: `wire` / `reg` / `logic` (e.g. `input wire clk`).
                    if matches!(
                        self.current().kind,
                        TokenKind::Wire | TokenKind::Reg | TokenKind::Logic
                    ) {
                        self.bump();
                    }
                    if self.current().kind == TokenKind::Signed {
                        self.bump();
                    }
                    if self.current().kind == TokenKind::LBracket {
                        self.bump();
                        let msb = self.parse_expression(0);
                        let lsb = if self.match_kind(TokenKind::Colon) {
                            self.parse_expression(0)
                        } else {
                            msb.clone()
                        };
                        let _ = self.match_kind(TokenKind::RBracket);
                        last_port_width =
                            Self::eval_port_packed_width(&msb, &lsb, &module_parameters);
                    } else {
                        last_port_width = 1;
                    }
                    Some(d)
                } else if self.current().kind == TokenKind::Identifier {
                    // Continuation after `input [7:0] a,` **or** legacy `(a, b, c)` with no directions.
                    if last_port_direction.is_none() {
                        last_port_width = 1;
                    }
                    last_port_direction.clone()
                } else {
                    self.error_at_current("expected port direction or name");
                    break 'ansi_ports;
                };

                let Some(port_name) = self.expect_identifier("expected port name") else {
                    break 'ansi_ports;
                };
                ports.push(Port {
                    direction,
                    name: port_name,
                    width: last_port_width,
                });

                if !self.match_kind(TokenKind::Comma) {
                    break 'ansi_ports;
                }
            }

            let _ = self.match_kind(TokenKind::RParen);
        }

        // skip tokens until ';' to get past header, but flag obviously bad cases
        let mut saw_semicolon = false;
        while self.current().kind != TokenKind::Semicolon
            && self.current().kind != TokenKind::Eof
        {
            if matches!(
                self.current().kind,
                TokenKind::Assign
                    | TokenKind::Wire
                    | TokenKind::Reg
                    | TokenKind::Logic
                    | TokenKind::Module
                    | TokenKind::Endmodule
            ) {
                self.error_at_current("expected ';' after module header");
                break;
            }
            self.bump();
        }
        if self.match_kind(TokenKind::Semicolon) {
            saw_semicolon = true;
        }
        if !saw_semicolon {
            // already reported above
        }

        // Each module has its own function/task table — clear any state from a
        // previous module so call resolution doesn't leak across boundaries.
        self.functions.clear();
        self.tasks.clear();

        let mut items = Vec::new();
        while self.current().kind != TokenKind::Endmodule
            && self.current().kind != TokenKind::Eof
        {
            if self.current().kind == TokenKind::Function {
                self.parse_function_decl();
            } else if self.current().kind == TokenKind::Task {
                self.parse_task_decl();
            } else if self.current().kind == TokenKind::Genvar {
                self.skip_genvar_statement();
            } else if self.current().kind == TokenKind::Generate {
                let mut inner = self.parse_generate_construct();
                items.append(&mut inner);
            } else if matches!(
                self.current().kind,
                TokenKind::Input | TokenKind::Output | TokenKind::Inout
            ) {
                if let Some(item) = self.parse_directional_net_decl() {
                    items.push(item);
                }
            } else if self.current().kind == TokenKind::Wire
                || self.current().kind == TokenKind::Reg
                || self.current().kind == TokenKind::Integer
                || self.current().kind == TokenKind::Logic
            {
                if let Some(item) = self.parse_net_decl() {
                    items.push(item);
                }
            } else if self.current().kind == TokenKind::Assign {
                if let Some(item) = self.parse_assign() {
                    items.push(item);
                }
            } else if self.current().kind == TokenKind::Always
                || self.current().kind == TokenKind::Initial
            {
                if let Some(item) = self.parse_always() {
                    items.push(item);
                }
            } else if self.current().kind == TokenKind::Parameter
                || self.current().kind == TokenKind::Localparam
            {
                self.bump();
                if let Some(assigns) = self.parse_param_assign_list_after_keyword() {
                    items.push(CstModuleItem::LocalParam { assignments: assigns });
                } else {
                    self.skip_to_semicolon();
                }
            } else if self.current().kind == TokenKind::Identifier {
                if let Some(item) = self.parse_instance_like() {
                    items.push(item);
                } else {
                    self.skip_to_semicolon();
                }
            } else {
                self.bump();
            }
        }
        let _ = self.match_kind(TokenKind::Endmodule);

        Some(CstModule {
            name,
            ports,
            module_parameters,
            path: self.file.path.clone(),
            items,
        })
    }

    fn skip_to_semicolon(&mut self) {
        while self.current().kind != TokenKind::Semicolon
            && self.current().kind != TokenKind::Eof
        {
            self.bump();
        }
        let _ = self.match_kind(TokenKind::Semicolon);
    }

    fn skip_genvar_statement(&mut self) {
        self.bump(); // genvar
        let _ = self.skip_to_semicolon();
    }

    /// Bit-width of `[msb:lsb]` for ANSI ports using module-parameter environment only.
    fn eval_port_packed_width(
        msb: &Expr,
        lsb: &Expr,
        module_parameters: &[(String, Expr)],
    ) -> usize {
        let known = crate::expr_const::resolve_local_param_values(module_parameters);
        let m = crate::expr_const::const_eval_param_expr(msb, &known).unwrap_or(1);
        let l = crate::expr_const::const_eval_param_expr(lsb, &known).unwrap_or(1);
        ((m - l).abs() as usize).saturating_add(1).max(1)
    }

    fn skip_to_endgenerate(&mut self) {
        while self.current().kind != TokenKind::Endgenerate
            && self.current().kind != TokenKind::Eof
        {
            self.bump();
        }
        let _ = self.match_kind(TokenKind::Endgenerate);
    }

    fn parse_module_parameter_list(&mut self) -> Vec<(String, Expr)> {
        let mut v = Vec::new();
        if !self.match_kind(TokenKind::Hash) {
            return v;
        }
        if !self.match_kind(TokenKind::LParen) {
            return v;
        }
        while self.current().kind != TokenKind::RParen && self.current().kind != TokenKind::Eof {
            if self.current().kind == TokenKind::Parameter {
                self.bump();
            }
            let Some(pname) = self.expect_identifier("expected parameter name") else {
                break;
            };
            if !self.match_kind(TokenKind::Eq) {
                self.error_at_current("expected `=` in module parameter list");
                while self.current().kind != TokenKind::RParen && self.current().kind != TokenKind::Eof {
                    self.bump();
                }
                break;
            }
            let rhs = self.parse_expression(0);
            v.push((pname, rhs));
            if self.match_kind(TokenKind::Comma) {
                continue;
            }
            break;
        }
        let _ = self.match_kind(TokenKind::RParen);
        v
    }

    fn parse_directional_net_decl(&mut self) -> Option<CstModuleItem> {
        if !matches!(
            self.current().kind,
            TokenKind::Input | TokenKind::Output | TokenKind::Inout
        ) {
            return None;
        }
        let decl_dir = self.current().lexeme.clone();
        self.bump();
        let mut item = self.parse_net_decl_core(NetKind::Wire)?;
        if let CstModuleItem::NetDecl { decl_dir: d, .. } = &mut item {
            *d = Some(decl_dir);
        }
        Some(item)
    }

    /// `generate … endgenerate` with `for (i=0; i<W; i=i+1) begin : … <one instance>; end`.
    fn parse_generate_construct(&mut self) -> Vec<CstModuleItem> {
        self.bump(); // generate
        // After `generate`, dispatch on the construct keyword. The body of
        // each construct may itself contain nested for / if / case via the
        // shared `parse_generate_item` helper.
        let items = self.parse_generate_items_until(TokenKind::Endgenerate);
        let _ = self.match_kind(TokenKind::Endgenerate);
        return items;
    }

    /// Parse zero or more generate-body items until `terminator`. Items may
    /// be `assign`, an instance, or another `for`/`if`/`case`.
    fn parse_generate_items_until(&mut self, terminator: TokenKind) -> Vec<CstModuleItem> {
        let mut items = Vec::new();
        while self.current().kind != terminator
            && self.current().kind != TokenKind::Eof
        {
            if let Some(item) = self.parse_generate_item() {
                items.push(item);
            } else {
                // Recover: skip an unrecognized token so we don't loop forever.
                self.bump();
            }
        }
        items
    }

    /// Parse a single generate-body item, dispatching on the leading token.
    /// Returns `None` for unrecognized starts so the caller can recover.
    fn parse_generate_item(&mut self) -> Option<CstModuleItem> {
        match self.current().kind {
            TokenKind::For => self.parse_generate_for_after_keyword(),
            TokenKind::If => self.parse_generate_if_after_keyword(),
            TokenKind::Case => self.parse_generate_case_after_keyword(),
            TokenKind::Assign => self.parse_assign(),
            TokenKind::Identifier => self.parse_instance_like(),
            TokenKind::Begin => {
                // Bare `begin ... end` group: treat as a transparent container
                // by parsing items and returning the first one. (Multiple
                // items in a bare begin block are rare in generate bodies; if
                // we need to support them we'd add a `Group` variant.)
                self.bump();
                if self.current().kind == TokenKind::Colon {
                    self.bump();
                    let _ = self.expect_identifier("expected block name after begin:");
                }
                let inner = self.parse_generate_items_until(TokenKind::End);
                let _ = self.match_kind(TokenKind::End);
                inner.into_iter().next()
            }
            _ => None,
        }
    }

    /// Parse `for (i = init; i < N; i = step) begin ... end`. Caller has
    /// already positioned past nothing — the `for` keyword is current.
    fn parse_generate_for_after_keyword(&mut self) -> Option<CstModuleItem> {
        self.bump(); // for
        if !self.match_kind(TokenKind::LParen) {
            return None;
        }
        let loop_var = self.expect_identifier("expected loop variable")?;
        if !self.match_kind(TokenKind::Eq) {
            return None;
        }
        let _init = self.parse_expression(0);
        if !self.match_kind(TokenKind::Semicolon) {
            return None;
        }
        let _iter = self.expect_identifier("expected loop variable")?;
        if !self.match_kind(TokenKind::Lt) {
            return None;
        }
        let upper_expr = self.parse_expression(0);
        if !self.match_kind(TokenKind::Semicolon) {
            return None;
        }
        let _lhs = self.expect_identifier("expected loop variable")?;
        if !self.match_kind(TokenKind::Eq) {
            return None;
        }
        let _step = self.parse_expression(0);
        if !self.match_kind(TokenKind::RParen) {
            return None;
        }
        if !self.match_kind(TokenKind::Begin) {
            return None;
        }
        if self.current().kind == TokenKind::Colon {
            self.bump();
            let _ = self.expect_identifier("expected block name after begin:");
        }
        let body = self.parse_generate_items_until(TokenKind::End);
        let _ = self.match_kind(TokenKind::End);

        // Shape detection — keep the existing IR paths for well-tested cases:
        // (a) exactly one instance → GenerateFor (single-instance variant)
        // (b) all continuous assigns → GenerateForAssigns
        // (c) anything else (nested generates, mixed) → GenerateForBody
        if body.len() == 1 {
            if let CstModuleItem::Instance {
                module_name,
                parameter_assignments,
                instance_name,
                connections,
            } = body[0].clone()
            {
                return Some(CstModuleItem::GenerateFor {
                    loop_var,
                    upper_expr,
                    module_name,
                    parameter_assignments,
                    instance_stem: instance_name,
                    connections,
                });
            }
        }
        let all_assigns = !body.is_empty()
            && body
                .iter()
                .all(|i| matches!(i, CstModuleItem::Assign { .. }));
        if all_assigns {
            let mut assigns = Vec::with_capacity(body.len());
            for item in body {
                if let CstModuleItem::Assign { target, expr, range } = item {
                    assigns.push((target, expr, range));
                }
            }
            return Some(CstModuleItem::GenerateForAssigns {
                loop_var,
                upper_expr,
                assigns,
            });
        }
        Some(CstModuleItem::GenerateForBody {
            loop_var,
            upper_expr,
            body,
        })
    }

    /// Parse `if (cond) <body> [else <body>]` where each body is either a
    /// single generate item or `begin ... end` group.
    fn parse_generate_if_after_keyword(&mut self) -> Option<CstModuleItem> {
        self.bump(); // if
        if !self.match_kind(TokenKind::LParen) {
            return None;
        }
        let cond = self.parse_expression(0);
        let _ = self.match_kind(TokenKind::RParen);
        let then_body = self.parse_generate_branch_body();
        let else_body = if self.current().kind == TokenKind::Else {
            self.bump();
            self.parse_generate_branch_body()
        } else {
            Vec::new()
        };
        Some(CstModuleItem::GenerateIf {
            cond,
            then_body,
            else_body,
        })
    }

    /// Body of a generate-if / generate-case arm: either `begin ... end`
    /// with multiple items, or a single bare item.
    fn parse_generate_branch_body(&mut self) -> Vec<CstModuleItem> {
        if self.current().kind == TokenKind::Begin {
            self.bump();
            if self.current().kind == TokenKind::Colon {
                self.bump();
                let _ = self.expect_identifier("expected block name after begin:");
            }
            let items = self.parse_generate_items_until(TokenKind::End);
            let _ = self.match_kind(TokenKind::End);
            items
        } else if let Some(item) = self.parse_generate_item() {
            vec![item]
        } else {
            Vec::new()
        }
    }

    /// Parse `case (scrutinee) <arms> [default: <body>] endcase`.
    fn parse_generate_case_after_keyword(&mut self) -> Option<CstModuleItem> {
        self.bump(); // case
        if !self.match_kind(TokenKind::LParen) {
            return None;
        }
        let scrutinee = self.parse_expression(0);
        let _ = self.match_kind(TokenKind::RParen);
        let mut arms = Vec::new();
        let mut default = Vec::new();
        while self.current().kind != TokenKind::Endcase
            && self.current().kind != TokenKind::Eof
        {
            if self.current().kind == TokenKind::Default {
                self.bump();
                let _ = self.match_kind(TokenKind::Colon);
                default = self.parse_generate_branch_body();
            } else {
                let value = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::Colon);
                let body = self.parse_generate_branch_body();
                arms.push((value, body));
            }
        }
        let _ = self.match_kind(TokenKind::Endcase);
        Some(CstModuleItem::GenerateCase {
            scrutinee,
            arms,
            default,
        })
    }

    /// Old entry point preserved as a stub for any callers; the body is
    /// fully handled by `parse_generate_construct` now.
    #[allow(dead_code)]
    fn _legacy_parse_generate_for_stub(&mut self) -> Vec<CstModuleItem> {
        // Begin original body for reference; never executed.
        if self.current().kind != TokenKind::For {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        self.bump(); // for
        if !self.match_kind(TokenKind::LParen) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let loop_var = match self.expect_identifier("expected loop variable") {
            Some(v) => v,
            None => {
                self.skip_to_endgenerate();
                return Vec::new();
            }
        };
        if !self.match_kind(TokenKind::Eq) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let _init = self.parse_expression(0);
        if !self.match_kind(TokenKind::Semicolon) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let _iter = match self.expect_identifier("expected loop variable") {
            Some(v) => v,
            None => {
                self.skip_to_endgenerate();
                return Vec::new();
            }
        };
        if !self.match_kind(TokenKind::Lt) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let upper_expr = self.parse_expression(0);
        if !self.match_kind(TokenKind::Semicolon) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let _lhs = match self.expect_identifier("expected loop variable") {
            Some(v) => v,
            None => {
                self.skip_to_endgenerate();
                return Vec::new();
            }
        };
        if !self.match_kind(TokenKind::Eq) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        let _step = self.parse_expression(0);
        if !self.match_kind(TokenKind::RParen) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        if !self.match_kind(TokenKind::Begin) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        if self.current().kind == TokenKind::Colon {
            self.bump();
            let _ = self.expect_identifier("expected block name after begin:");
        }
        // Body discriminator: `assign` => generate-for-of-assigns, anything
        // else => fall back to the existing single-instance form.
        if self.current().kind == TokenKind::Assign {
            let mut assigns = Vec::new();
            while self.current().kind == TokenKind::Assign {
                if let Some(CstModuleItem::Assign { target, expr, range }) = self.parse_assign() {
                    assigns.push((target, expr, range));
                }
            }
            if !self.match_kind(TokenKind::End) {
                self.skip_to_endgenerate();
                return Vec::new();
            }
            if !self.match_kind(TokenKind::Endgenerate) {
                self.skip_to_endgenerate();
                return Vec::new();
            }
            return vec![CstModuleItem::GenerateForAssigns {
                loop_var,
                upper_expr,
                assigns,
            }];
        }
        let inst = match self.parse_instance_like() {
            Some(CstModuleItem::Instance {
                module_name,
                parameter_assignments,
                instance_name,
                connections,
            }) => (module_name, parameter_assignments, instance_name, connections),
            _ => {
                self.skip_to_endgenerate();
                return Vec::new();
            }
        };
        if !self.match_kind(TokenKind::End) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        if !self.match_kind(TokenKind::Endgenerate) {
            self.skip_to_endgenerate();
            return Vec::new();
        }
        vec![CstModuleItem::GenerateFor {
            loop_var,
            upper_expr,
            module_name: inst.0.clone(),
            parameter_assignments: inst.1.clone(),
            instance_stem: inst.2.clone(),
            connections: inst.3.clone(),
        }]
    }

    /// After consuming the `parameter` / `localparam` keyword: optional `[high:low]`, then
    /// `name = expr` comma-lists (IEEE 1364).
    fn parse_param_assign_list_after_keyword(&mut self) -> Option<Vec<(String, Expr)>> {
        if self.current().kind == TokenKind::LBracket {
            while self.current().kind != TokenKind::RBracket && self.current().kind != TokenKind::Eof {
                self.bump();
            }
            let _ = self.match_kind(TokenKind::RBracket);
        }
        let mut pairs = Vec::new();
        loop {
            let name = self.expect_identifier("expected parameter name")?;
            if !self.match_kind(TokenKind::Eq) {
                self.error_at_current("expected `=` in parameter/localparam declaration");
                return None;
            }
            let rhs = self.parse_expression(0);
            pairs.push((name, rhs));
            if self.match_kind(TokenKind::Comma) {
                continue;
            }
            break;
        }
        let _ = self.match_kind(TokenKind::Semicolon);
        Some(pairs)
    }

    fn parse_net_decl(&mut self) -> Option<CstModuleItem> {
        let kind = match self.current().kind {
            TokenKind::Wire => NetKind::Wire,
            TokenKind::Integer => NetKind::Reg,
            TokenKind::Logic => NetKind::Reg,
            _ => NetKind::Reg,
        };
        self.bump();
        self.parse_net_decl_core(kind)
    }

    fn parse_net_decl_core(&mut self, kind: NetKind) -> Option<CstModuleItem> {
        let mut width = if kind == NetKind::Reg && self.tokens[self.pos - 1].lexeme == "integer" {
            32
        } else {
            1
        };

        if self.current().kind == TokenKind::Signed {
            self.bump();
        }

        let mut packed_dim: Option<(Expr, Expr)> = None;
        if self.current().kind == TokenKind::LBracket {
            self.bump(); // [
            let msb = self.parse_expression(0);
            let lsb = if self.match_kind(TokenKind::Colon) {
                self.parse_expression(0)
            } else {
                msb.clone()
            };
            let _ = self.match_kind(TokenKind::RBracket);
            packed_dim = Some((msb, lsb));
            width = 1;
        }

        let mut names = Vec::new();
        let mut unpacked_stems: Vec<(String, Expr, Expr)> = Vec::new();
        loop {
            let stem = match self.expect_identifier("expected signal name") {
                Some(n) => n,
                None => break,
            };
            if self.current().kind == TokenKind::LBracket {
                self.bump();
                let hi = self.parse_expression(0);
                let lo = if self.match_kind(TokenKind::Colon) {
                    self.parse_expression(0)
                } else {
                    hi.clone()
                };
                let _ = self.match_kind(TokenKind::RBracket);
                unpacked_stems.push((stem.clone(), hi, lo));
                names.push(stem);
            } else {
                names.push(stem);
            }
            if !self.match_kind(TokenKind::Comma) {
                break;
            }
        }
        self.skip_to_semicolon();
        if names.is_empty() {
            None
        } else {
            Some(CstModuleItem::NetDecl {
                kind,
                packed_dim,
                width,
                names,
                unpacked_stems,
                decl_dir: None,
            })
        }
    }

    fn parse_assign(&mut self) -> Option<CstModuleItem> {
        let start = self.current().offset as u32;
        self.bump(); // consume 'assign'
        let reg = match self.expect_identifier("expected left-hand side of assign") {
            Some(name) => name,
            None => {
                self.skip_to_semicolon();
                return None;
            }
        };
        let target = self.parse_assign_target_suffix(reg);
        let _ = self.match_kind(TokenKind::Eq);
        let expr = self.parse_expression(0);
        // Capture the terminating `;`'s end before consuming it; falls back
        // to the prior token's end when recovery landed on something else.
        let end = if self.current().kind == TokenKind::Semicolon {
            self.current().end as u32
        } else {
            self.current().offset as u32
        };
        self.skip_to_semicolon();
        Some(CstModuleItem::Assign {
            target,
            expr,
            range: (start, end),
        })
    }

    fn parse_instance_like(&mut self) -> Option<CstModuleItem> {
        let module_name = self.current().lexeme.clone();
        self.bump();
        let mut parameter_assignments = Vec::new();
        if self.match_kind(TokenKind::Hash) {
            if self.match_kind(TokenKind::LParen) {
                while self.current().kind != TokenKind::RParen
                    && self.current().kind != TokenKind::Eof
                {
                    if !self.match_kind(TokenKind::Dot) {
                        break;
                    }
                    let pname = match self.expect_identifier("expected parameter name after `.`") {
                        Some(n) => n,
                        None => break,
                    };
                    if !self.match_kind(TokenKind::LParen) {
                        break;
                    }
                    let rhs = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::RParen);
                    parameter_assignments.push((pname, rhs));
                    let _ = self.match_kind(TokenKind::Comma);
                }
                let _ = self.match_kind(TokenKind::RParen);
            }
        }
        let instance_name =
            match self.expect_identifier("expected instance name after module name") {
                Some(n) => n,
                None => return None,
            };

        let mut connections = Vec::new();
        if self.match_kind(TokenKind::LParen) {
            while self.current().kind != TokenKind::RParen
                && self.current().kind != TokenKind::Eof
            {
                if self.current().kind == TokenKind::Dot {
                    let conn_start = self.current().offset as u32;
                    self.bump(); // consume '.'
                    if let Some(port_name) = self.expect_identifier("expected port name") {
                        let _ = self.match_kind(TokenKind::LParen);
                        let expr = self.parse_expression(0);
                        let conn_end = if self.current().kind == TokenKind::RParen {
                            self.current().end as u32
                        } else {
                            self.current().offset as u32
                        };
                        let _ = self.match_kind(TokenKind::RParen);
                        connections.push(PortConnection {
                            port_name: Some(port_name),
                            expr,
                            range: (conn_start, conn_end),
                        });
                    }
                } else {
                    let conn_start = self.current().offset as u32;
                    let expr = self.parse_expression(0);
                    // `self.current()` now points at `,` or `)` (or recovery
                    // target); its offset is the exclusive end of the expr.
                    let conn_end = self.current().offset as u32;
                    connections.push(PortConnection {
                        port_name: None,
                        expr,
                        range: (conn_start, conn_end.max(conn_start)),
                    });
                }
                if !self.match_kind(TokenKind::Comma) {
                    break;
                }
            }
            let _ = self.match_kind(TokenKind::RParen);
        }
        self.skip_to_semicolon();
        Some(CstModuleItem::Instance {
            module_name,
            parameter_assignments,
            instance_name,
            connections,
        })
    }

    fn parse_always(&mut self) -> Option<CstModuleItem> {
        let is_initial = self.current().kind == TokenKind::Initial;
        self.bump(); // consume 'always' or 'initial'
        if is_initial {
            let body = self.parse_stmt_block();
            return Some(CstModuleItem::Initial { body });
        }
        // `always #delay stmt` — procedural delay before the statement (e.g. clock generators).
        if self.current().kind == TokenKind::Hash {
            let delay_start = self.cur_start();
            let ticks = self.parse_delay_numeric_after_hash();
            let delay_end = self.last_end();
            let mut body = CstBlock::new();
            body.push(CstStmt::Delay(ticks), (delay_start, delay_end));
            let stmt_start = self.cur_start();
            if let Some(s) = self.parse_stmt() {
                let stmt_end = self.last_end();
                body.push(s, (stmt_start, stmt_end));
            }
            return Some(CstModuleItem::Always {
                sensitivity: Sensitivity::Star,
                body,
            });
        }
        let sensitivity = self.parse_sensitivity();
        let body = self.parse_stmt_block();
        Some(CstModuleItem::Always { sensitivity, body })
    }

    /// `#` then integer or real delay (`#5`, `#0.5`); does not consume trailing `;`.
    fn parse_delay_numeric_after_hash(&mut self) -> DelayRational {
        if self.current().kind != TokenKind::Hash {
            return DelayRational::ZERO;
        }
        self.bump();
        Self::delay_from_delay_lexeme(&self.parse_delay_lexeme_tokens())
    }

    /// Reads delay literal after `#` already consumed: one number, optional `.` fraction.
    fn parse_delay_lexeme_tokens(&mut self) -> String {
        if self.current().kind != TokenKind::Number {
            return String::new();
        }
        let mut s = self.current().lexeme.clone();
        self.bump();
        if self.current().kind == TokenKind::Dot {
            self.bump();
            if self.current().kind == TokenKind::Number {
                s.push('.');
                s.push_str(&self.current().lexeme);
                self.bump();
            }
        }
        s
    }

    fn delay_from_delay_lexeme(s: &str) -> DelayRational {
        DelayRational::from_delay_lexeme(s)
    }

    fn parse_sensitivity(&mut self) -> Sensitivity {
        if !self.match_kind(TokenKind::At) {
            return Sensitivity::Star;
        }
        // IEEE 1364: `always @*` (implicit event list) without parentheses.
        if self.current().kind == TokenKind::Star {
            self.bump();
            return Sensitivity::Star;
        }
        if !self.match_kind(TokenKind::LParen) {
            return Sensitivity::Star;
        }
        // Check for @(*)
        if self.current().kind == TokenKind::Star {
            self.bump();
            let _ = self.match_kind(TokenKind::RParen);
            return Sensitivity::Star;
        }
        let mut edges = Vec::new();
        loop {
            let edge = if self.current().kind == TokenKind::Posedge {
                self.bump();
                EdgeKind::Posedge
            } else if self.current().kind == TokenKind::Negedge {
                self.bump();
                EdgeKind::Negedge
            } else {
                EdgeKind::Level
            };
            if let Some(sig) = self.expect_identifier("expected signal in sensitivity list") {
                edges.push(SensEdge { edge, signal: sig });
            } else {
                break;
            }
            // 'or' or ','  separates entries
            if self.current().kind == TokenKind::Identifier && self.current().lexeme == "or" {
                self.bump();
            } else if self.current().kind == TokenKind::Comma {
                self.bump();
            } else {
                break;
            }
        }
        let _ = self.match_kind(TokenKind::RParen);
        Sensitivity::EdgeList(edges)
    }

    fn parse_stmt_block(&mut self) -> CstBlock {
        let mut block = CstBlock::new();
        if self.match_kind(TokenKind::Begin) {
            while self.current().kind != TokenKind::End
                && self.current().kind != TokenKind::Eof
            {
                let start = self.cur_start();
                let before = self.pos;
                if let Some(s) = self.parse_stmt() {
                    let end = self.last_end();
                    block.push(s, (start, end));
                } else if self.pos == before {
                    // Guarantee progress: skip the current token to avoid infinite loop on
                    // malformed statements that don't consume anything.
                    self.bump();
                }
            }
            let _ = self.match_kind(TokenKind::End);
        } else {
            let start = self.cur_start();
            if let Some(s) = self.parse_stmt() {
                let end = self.last_end();
                block.push(s, (start, end));
            }
        }
        block
    }

    /// After the register/net identifier: optional `[bit]` or `[msb:lsb]`.
    fn parse_assign_target_suffix(&mut self, reg: String) -> AssignTarget {
        if self.current().kind == TokenKind::LBracket {
            self.bump();
            let msb = self.parse_expression(0);
            if self.match_kind(TokenKind::Colon) {
                let lsb = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::RBracket);
                AssignTarget::PartSelect { reg, msb, lsb }
            } else {
                let _ = self.match_kind(TokenKind::RBracket);
                AssignTarget::BitSelect { reg, index: msb }
            }
        } else {
            AssignTarget::Whole(reg)
        }
    }

    /// Parse a concat-LHS like `{a, b[3:0], c[2]}` already positioned at the
    /// opening `{`. Each component is parsed as a simple `AssignTarget` (which
    /// may itself recurse into a nested concat). The result is MSB-first to
    /// match Verilog concat semantics.
    fn parse_concat_lhs(&mut self) -> Option<AssignTarget> {
        if !self.match_kind(TokenKind::LBrace) {
            return None;
        }
        let mut parts = Vec::new();
        loop {
            if self.current().kind == TokenKind::RBrace {
                break;
            }
            let part = if self.current().kind == TokenKind::LBrace {
                self.parse_concat_lhs()?
            } else if self.current().kind == TokenKind::Identifier {
                let reg = self.current().lexeme.clone();
                self.bump();
                self.parse_assign_target_suffix(reg)
            } else {
                // Bad token in concat-LHS — recover to the closing brace or semi.
                self.skip_to_semicolon();
                return None;
            };
            parts.push(part);
            if !self.match_kind(TokenKind::Comma) {
                break;
            }
        }
        let _ = self.match_kind(TokenKind::RBrace);
        Some(AssignTarget::Concat(parts))
    }

    fn parse_stmt(&mut self) -> Option<CstStmt> {
        match self.current().kind {
            TokenKind::Hash => {
                let ticks = self.parse_delay_numeric_after_hash();
                let _ = self.match_kind(TokenKind::Semicolon);
                return Some(CstStmt::Delay(ticks));
            }
            _ if self.current().kind == TokenKind::Identifier
                && self.current().lexeme.starts_with('$') =>
            {
                let name = self.current().lexeme.clone();
                self.bump();
                let mut args = Vec::new();
                if self.match_kind(TokenKind::LParen) {
                    if self.current().kind != TokenKind::RParen {
                        args.push(self.parse_expression(0));
                        while self.match_kind(TokenKind::Comma) {
                            args.push(self.parse_expression(0));
                        }
                    }
                    let _ = self.match_kind(TokenKind::RParen);
                }
                let _ = self.match_kind(TokenKind::Semicolon);
                return Some(CstStmt::SystemTask { name, args });
            }
            _ => {}
        }
        match self.current().kind {
            TokenKind::If => {
                self.bump();
                let _ = self.match_kind(TokenKind::LParen);
                let cond = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::RParen);
                let then_body = self.parse_stmt_block();
                let else_body = if self.match_kind(TokenKind::Else) {
                    self.parse_stmt_block()
                } else {
                    CstBlock::new()
                };
                Some(CstStmt::IfElse { cond, then_body, else_body })
            }
            TokenKind::Case | TokenKind::Casez | TokenKind::Casex => {
                let kind = match self.current().kind {
                    TokenKind::Casez => CaseKind::Z,
                    TokenKind::Casex => CaseKind::X,
                    _ => CaseKind::Plain,
                };
                self.bump();
                let _ = self.match_kind(TokenKind::LParen);
                let expr = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::RParen);
                let mut arms = Vec::new();
                let mut default = CstBlock::new();
                while self.current().kind != TokenKind::Endcase
                    && self.current().kind != TokenKind::Eof
                {
                    if self.current().kind == TokenKind::Default {
                        self.bump();
                        let _ = self.match_kind(TokenKind::Colon);
                        default = self.parse_stmt_block();
                    } else {
                        let value = self.parse_expression(0);
                        let _ = self.match_kind(TokenKind::Colon);
                        let body = self.parse_stmt_block();
                        arms.push(CaseArm { value, body });
                    }
                }
                let _ = self.match_kind(TokenKind::Endcase);
                Some(CstStmt::Case { kind, expr, arms, default })
            }
            TokenKind::For => {
                self.bump();
                let _ = self.match_kind(TokenKind::LParen);
                // init: var = expr
                let init_var = self.expect_identifier("expected loop variable")?;
                let _ = self.match_kind(TokenKind::Eq);
                let init_val = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::Semicolon);
                // cond
                let cond = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::Semicolon);
                // step: var = expr
                let step_var = self.expect_identifier("expected step variable")?;
                let _ = self.match_kind(TokenKind::Eq);
                let step_expr = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::RParen);
                let body = self.parse_stmt_block();
                Some(CstStmt::For { init_var, init_val, cond, step_var, step_expr, body })
            }
            TokenKind::LBrace => {
                // Concat-LHS: `{a, b[3:0], c} = rhs;` or `{...} <= rhs;`
                // We never start an expression statement with `{`, so this
                // unambiguously begins a concat-LHS assignment.
                let target = self.parse_concat_lhs()?;
                if self.current().kind == TokenKind::Le {
                    self.bump();
                    let rhs = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::Semicolon);
                    Some(CstStmt::NonBlockingAssign { target, rhs })
                } else if self.match_kind(TokenKind::Eq) {
                    let rhs = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::Semicolon);
                    Some(CstStmt::BlockingAssign { target, rhs })
                } else {
                    self.skip_to_semicolon();
                    None
                }
            }
            TokenKind::Identifier => {
                let reg = self.current().lexeme.clone();
                // Task call: identifier(args); — only when the name matches a
                // module-local task. We peek without bumping so we can fall
                // back to the assign path for non-tasks.
                if self.pos + 1 < self.tokens.len()
                    && self.tokens[self.pos + 1].kind == TokenKind::LParen
                    && self.tasks.iter().any(|t| t.name == reg)
                {
                    self.bump(); // identifier
                    self.bump(); // (
                    let mut actuals = Vec::new();
                    if self.current().kind != TokenKind::RParen {
                        actuals.push(self.parse_expression(0));
                        while self.match_kind(TokenKind::Comma) {
                            actuals.push(self.parse_expression(0));
                        }
                    }
                    let _ = self.match_kind(TokenKind::RParen);
                    let _ = self.match_kind(TokenKind::Semicolon);
                    let def = self
                        .tasks
                        .iter()
                        .find(|t| t.name == reg)
                        .expect("task present by name")
                        .clone();
                    let subs: Vec<(String, Expr)> =
                        def.args.iter().cloned().zip(actuals.into_iter()).collect();
                    return Some(subst_stmt_multi(def.body_stmt, &subs));
                }
                self.bump();
                let target = self.parse_assign_target_suffix(reg);
                if self.current().kind == TokenKind::Le {
                    // Non-blocking assignment: lhs <= rhs
                    self.bump();
                    let rhs = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::Semicolon);
                    Some(CstStmt::NonBlockingAssign { target, rhs })
                } else if self.match_kind(TokenKind::Eq) {
                    // Blocking assignment: lhs = rhs
                    let rhs = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::Semicolon);
                    Some(CstStmt::BlockingAssign { target, rhs })
                } else {
                    self.skip_to_semicolon();
                    None
                }
            }
            _ => {
                self.bump();
                None
            }
        }
    }

    // ── Expression parsing with full Verilog precedence ──────────────

    /// Pratt-style expression parser. Precedence table (low → high):
    ///  1: ||         (LogOr)
    ///  2: &&         (LogAnd)
    ///  3: |          (Or)
    ///  4: ^          (Xor)
    ///  5: &          (And)
    ///  6: == !=      (Eq / Ne)
    ///  7: < <= > >=  (Comparison)
    ///  8: << >>      (Shift)
    ///  9: + -        (Add / Sub)
    /// 10: * / %      (Mul / Div / Mod)
    fn parse_expression(&mut self, min_prec: u8) -> Expr {
        let mut left = self.parse_unary();
        loop {
            let (op, prec) = match self.current().kind {
                TokenKind::LogOr  => (BinaryOp::LogOr, 1),
                TokenKind::LogAnd => (BinaryOp::LogAnd, 2),
                TokenKind::Pipe   => (BinaryOp::Or, 3),
                TokenKind::Caret  => (BinaryOp::Xor, 4),
                TokenKind::Amp    => (BinaryOp::And, 5),
                TokenKind::EqEq   => (BinaryOp::Eq, 6),
                TokenKind::Ne     => (BinaryOp::Ne, 6),
                TokenKind::Lt     => (BinaryOp::Lt, 7),
                TokenKind::Le     => (BinaryOp::Le, 7),
                TokenKind::Gt     => (BinaryOp::Gt, 7),
                TokenKind::Ge     => (BinaryOp::Ge, 7),
                TokenKind::Shl    => (BinaryOp::Shl, 8),
                TokenKind::Shr    => (BinaryOp::Shr, 8),
                TokenKind::Ashr   => (BinaryOp::Ashr, 8),
                TokenKind::Plus   => (BinaryOp::Add, 9),
                TokenKind::Minus  => (BinaryOp::Sub, 9),
                TokenKind::Star   => (BinaryOp::Mul, 10),
                TokenKind::Slash  => (BinaryOp::Div, 10),
                TokenKind::Percent => (BinaryOp::Mod, 10),
                // Ternary handled inline
                TokenKind::Question => {
                    if min_prec > 0 {
                        break;
                    }
                    self.bump();
                    let then_expr = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::Colon);
                    let else_expr = self.parse_expression(0);
                    left = Expr::Ternary {
                        cond: Box::new(left),
                        then_expr: Box::new(then_expr),
                        else_expr: Box::new(else_expr),
                    };
                    continue;
                }
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let right = self.parse_expression(prec + 1);
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        left
    }

    /// Parse `function [W-1:0] name; input [...] a; input [...] b; begin
    /// name = <expr>; end endfunction` and stash in [`Self::functions`] for
    /// inlining at expression parse time. Bodies more complex than a single
    /// `name = expr;` are flagged and skipped.
    fn parse_function_decl(&mut self) {
        self.bump(); // function
        // Optional return width (we only need its presence; widths are
        // currently inferred from the assigned expression).
        if self.current().kind == TokenKind::Signed {
            self.bump();
        }
        if self.current().kind == TokenKind::LBracket {
            self.bump();
            while self.current().kind != TokenKind::RBracket
                && self.current().kind != TokenKind::Eof
            {
                self.bump();
            }
            let _ = self.match_kind(TokenKind::RBracket);
        }
        let name = match self.expect_identifier("expected function name") {
            Some(n) => n,
            None => {
                self.skip_to_endfunction();
                return;
            }
        };
        let _ = self.match_kind(TokenKind::Semicolon);
        // Argument declarations: zero or more `input [W-1:0] x;` lines.
        let mut args: Vec<String> = Vec::new();
        while self.current().kind == TokenKind::Input {
            self.bump();
            if matches!(
                self.current().kind,
                TokenKind::Wire | TokenKind::Reg | TokenKind::Logic
            ) {
                self.bump();
            }
            if self.current().kind == TokenKind::Signed {
                self.bump();
            }
            if self.current().kind == TokenKind::LBracket {
                self.bump();
                while self.current().kind != TokenKind::RBracket
                    && self.current().kind != TokenKind::Eof
                {
                    self.bump();
                }
                let _ = self.match_kind(TokenKind::RBracket);
            }
            if let Some(arg) = self.expect_identifier("expected function argument name") {
                args.push(arg);
            }
            let _ = self.match_kind(TokenKind::Semicolon);
        }
        // Body: optional `begin` ... `end` wrapping a single `name = expr;`.
        let saw_begin = self.match_kind(TokenKind::Begin);
        // Expect `name = expr;` — the assignment to the function-name carries
        // the return value.
        let body_expr = if self.current().kind == TokenKind::Identifier
            && self.current().lexeme == name
        {
            self.bump();
            if !self.match_kind(TokenKind::Eq) {
                self.skip_to_endfunction();
                return;
            }
            let e = self.parse_expression(0);
            let _ = self.match_kind(TokenKind::Semicolon);
            e
        } else {
            // Unsupported richer body — record and skip the rest.
            self.error_at_current(&format!(
                "function `{name}` body must be `begin {name} = <expr>; end` for now"
            ));
            self.skip_to_endfunction();
            return;
        };
        if saw_begin {
            let _ = self.match_kind(TokenKind::End);
        }
        let _ = self.match_kind(TokenKind::Endfunction);
        self.functions.push(FuncDef {
            name,
            args,
            body_expr,
        });
    }

    fn skip_to_endfunction(&mut self) {
        while self.current().kind != TokenKind::Endfunction
            && self.current().kind != TokenKind::Eof
        {
            self.bump();
        }
        let _ = self.match_kind(TokenKind::Endfunction);
    }

    /// Parse `task name; input [...] a; begin <stmt>; end endtask` and store
    /// in [`Self::tasks`] for inlining at statement parse time.
    fn parse_task_decl(&mut self) {
        self.bump(); // task
        let name = match self.expect_identifier("expected task name") {
            Some(n) => n,
            None => {
                self.skip_to_endtask();
                return;
            }
        };
        let _ = self.match_kind(TokenKind::Semicolon);
        let mut args: Vec<String> = Vec::new();
        while self.current().kind == TokenKind::Input
            || self.current().kind == TokenKind::Output
            || self.current().kind == TokenKind::Inout
        {
            self.bump(); // direction (only `input` is meaningfully used here)
            if matches!(
                self.current().kind,
                TokenKind::Wire | TokenKind::Reg | TokenKind::Logic
            ) {
                self.bump();
            }
            if self.current().kind == TokenKind::Signed {
                self.bump();
            }
            if self.current().kind == TokenKind::LBracket {
                self.bump();
                while self.current().kind != TokenKind::RBracket
                    && self.current().kind != TokenKind::Eof
                {
                    self.bump();
                }
                let _ = self.match_kind(TokenKind::RBracket);
            }
            if let Some(arg) = self.expect_identifier("expected task argument name") {
                args.push(arg);
            }
            let _ = self.match_kind(TokenKind::Semicolon);
        }
        let saw_begin = self.match_kind(TokenKind::Begin);
        let body_stmt = match self.parse_stmt() {
            Some(s) => s,
            None => {
                self.error_at_current(&format!("task `{name}` body could not be parsed"));
                self.skip_to_endtask();
                return;
            }
        };
        if saw_begin {
            let _ = self.match_kind(TokenKind::End);
        }
        let _ = self.match_kind(TokenKind::Endtask);
        self.tasks.push(TaskDef {
            name,
            args,
            body_stmt,
        });
    }

    fn skip_to_endtask(&mut self) {
        while self.current().kind != TokenKind::Endtask
            && self.current().kind != TokenKind::Eof
        {
            self.bump();
        }
        let _ = self.match_kind(TokenKind::Endtask);
    }

    fn parse_unary(&mut self) -> Expr {
        match self.current().kind {
            TokenKind::Tilde => {
                self.bump();
                let operand = self.parse_unary();
                Expr::Unary { op: UnaryOp::Not, operand: Box::new(operand) }
            }
            TokenKind::Bang => {
                self.bump();
                let operand = self.parse_unary();
                Expr::Unary { op: UnaryOp::LogNot, operand: Box::new(operand) }
            }
            TokenKind::Minus => {
                self.bump();
                let operand = self.parse_unary();
                Expr::Unary { op: UnaryOp::Neg, operand: Box::new(operand) }
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Expr {
        let expr = match self.current().kind {
            TokenKind::Identifier => {
                let name = self.current().lexeme.clone();
                self.bump();
                if name == "$signed" && self.match_kind(TokenKind::LParen) {
                    let arg = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::RParen);
                    Expr::Signed(Box::new(arg))
                } else if name == "$clog2" && self.match_kind(TokenKind::LParen) {
                    let arg = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::RParen);
                    Expr::Clog2(Box::new(arg))
                } else if self.current().kind == TokenKind::LParen
                    && self.functions.iter().any(|f| f.name == name)
                {
                    // Function call — parse actuals, substitute formals,
                    // return the inlined body expression.
                    self.bump(); // (
                    let mut actuals = Vec::new();
                    if self.current().kind != TokenKind::RParen {
                        actuals.push(self.parse_expression(0));
                        while self.match_kind(TokenKind::Comma) {
                            actuals.push(self.parse_expression(0));
                        }
                    }
                    let _ = self.match_kind(TokenKind::RParen);
                    let def = self
                        .functions
                        .iter()
                        .find(|f| f.name == name)
                        .expect("function present by name in table")
                        .clone();
                    // Right-pad / left-truncate to formal arity. Mismatch is
                    // diagnosed but not fatal — extra actuals are ignored,
                    // missing ones leave the formal name unbound.
                    let mut subs: Vec<(String, Expr)> =
                        def.args.iter().cloned().zip(actuals.into_iter()).collect();
                    // If too few actuals, leave the unbound formals unreplaced
                    // (they'll resolve to the surrounding scope's identifier).
                    let _ = &mut subs;
                    subst_expr_multi(def.body_expr, &subs)
                } else {
                    Expr::Ident(name)
                }
            }
            TokenKind::Number => {
                let lit = self.current().lexeme.clone();
                self.bump();
                Expr::Number(lit)
            }
            TokenKind::LParen => {
                self.bump();
                let expr = self.parse_expression(0);
                let _ = self.match_kind(TokenKind::RParen);
                expr
            }
            TokenKind::LBrace => {
                self.bump();
                // Detect replication: {N{expr}}
                // Pattern: number followed by LBrace
                if self.current().kind == TokenKind::Number
                    && self.pos + 1 < self.tokens.len()
                    && self.tokens[self.pos + 1].kind == TokenKind::LBrace
                {
                    let count_str = self.current().lexeme.clone();
                    let count = count_str.parse::<usize>().unwrap_or(1);
                    self.bump(); // consume the number
                    self.bump(); // consume the inner '{'
                    let inner = self.parse_expression(0);
                    let _ = self.match_kind(TokenKind::RBrace); // inner '}'
                    let _ = self.match_kind(TokenKind::RBrace); // outer '}'
                    let exprs = vec![inner; count];
                    Expr::Concat(exprs)
                } else {
                    let mut exprs = Vec::new();
                    if self.current().kind != TokenKind::RBrace {
                        exprs.push(self.parse_expression(0));
                        while self.match_kind(TokenKind::Comma) {
                            exprs.push(self.parse_expression(0));
                        }
                    }
                    let _ = self.match_kind(TokenKind::RBrace);
                    Expr::Concat(exprs)
                }
            }
            _ => {
                self.bump();
                Expr::Ident("<unsupported>".to_string())
            }
        };
        self.parse_index_suffixes(expr)
    }

    fn parse_index_suffixes(&mut self, mut expr: Expr) -> Expr {
        while self.current().kind == TokenKind::LBracket {
            self.bump();
            let msb = self.parse_expression(0);
            let lsb = if self.match_kind(TokenKind::Colon) {
                Some(Box::new(self.parse_expression(0)))
            } else {
                None
            };
            let _ = self.match_kind(TokenKind::RBracket);
            expr = Expr::Index {
                base: Box::new(expr),
                msb: Box::new(msb),
                lsb,
            };
        }
        expr
    }
}

/// Replace every occurrence of the identifier `loop_var` in an [`AssignTarget`]
/// with the integer literal `k`. Used by IR lowering when unrolling a
/// generate-for of assigns. Mirrors [`subst_expr_loop_var`] for the LHS.
pub(crate) fn subst_target_loop_var(t: AssignTarget, loop_var: &str, k: i64) -> AssignTarget {
    match t {
        AssignTarget::Whole(name) => AssignTarget::Whole(name),
        AssignTarget::BitSelect { reg, index } => AssignTarget::BitSelect {
            reg,
            index: subst_expr_loop_var(index, loop_var, k),
        },
        AssignTarget::PartSelect { reg, msb, lsb } => AssignTarget::PartSelect {
            reg,
            msb: subst_expr_loop_var(msb, loop_var, k),
            lsb: subst_expr_loop_var(lsb, loop_var, k),
        },
        AssignTarget::Concat(parts) => AssignTarget::Concat(
            parts
                .into_iter()
                .map(|p| subst_target_loop_var(p, loop_var, k))
                .collect(),
        ),
    }
}

pub(crate) fn subst_expr_loop_var(e: Expr, loop_var: &str, k: i64) -> Expr {
    match e {
        Expr::Ident(s) if s == loop_var => Expr::Number(format!("{k}")),
        Expr::Ident(s) => Expr::Ident(s),
        Expr::Number(n) => Expr::Number(n),
        Expr::Binary { op, left, right } => Expr::Binary {
            op,
            left: Box::new(subst_expr_loop_var(*left, loop_var, k)),
            right: Box::new(subst_expr_loop_var(*right, loop_var, k)),
        },
        Expr::Unary { op, operand } => Expr::Unary {
            op,
            operand: Box::new(subst_expr_loop_var(*operand, loop_var, k)),
        },
        Expr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => Expr::Ternary {
            cond: Box::new(subst_expr_loop_var(*cond, loop_var, k)),
            then_expr: Box::new(subst_expr_loop_var(*then_expr, loop_var, k)),
            else_expr: Box::new(subst_expr_loop_var(*else_expr, loop_var, k)),
        },
        Expr::Concat(exprs) => Expr::Concat(
            exprs
                .into_iter()
                .map(|e| subst_expr_loop_var(e, loop_var, k))
                .collect(),
        ),
        Expr::Index { base, msb, lsb } => Expr::Index {
            base: Box::new(subst_expr_loop_var(*base, loop_var, k)),
            msb: Box::new(subst_expr_loop_var(*msb, loop_var, k)),
            lsb: lsb.map(|x| Box::new(subst_expr_loop_var(*x, loop_var, k))),
        },
        Expr::Clog2(a) => Expr::Clog2(Box::new(subst_expr_loop_var(*a, loop_var, k))),
        Expr::Signed(a) => Expr::Signed(Box::new(subst_expr_loop_var(*a, loop_var, k))),
    }
}

pub(crate) fn subst_port_connections(conns: &[PortConnection], loop_var: &str, k: i64) -> Vec<PortConnection> {
    conns
        .iter()
        .map(|c| PortConnection {
            port_name: c.port_name.clone(),
            expr: subst_expr_loop_var(c.expr.clone(), loop_var, k),
            range: c.range,
        })
        .collect()
}

fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in text.chars().enumerate() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}


/// Substitute `loop_var` -> `k` everywhere inside a [`CstModuleItem`].
/// Used by the IR generate-body elaborator. Shadowing is respected for
/// nested generate-for constructs: if the inner loop binds the same name,
/// its body is left untouched (the inner binding shadows).
pub(crate) fn subst_module_item_loop_var(
    item: CstModuleItem,
    loop_var: &str,
    k: i64,
) -> CstModuleItem {
    match item {
        CstModuleItem::Assign { target, expr, range } => CstModuleItem::Assign {
            target: subst_target_loop_var(target, loop_var, k),
            expr: subst_expr_loop_var(expr, loop_var, k),
            range,
        },
        CstModuleItem::Instance {
            module_name,
            parameter_assignments,
            instance_name,
            connections,
        } => CstModuleItem::Instance {
            module_name,
            parameter_assignments: parameter_assignments
                .into_iter()
                .map(|(n, e)| (n, subst_expr_loop_var(e, loop_var, k)))
                .collect(),
            instance_name,
            connections: subst_port_connections(&connections, loop_var, k),
        },
        CstModuleItem::GenerateFor {
            loop_var: inner_var,
            upper_expr,
            module_name,
            parameter_assignments,
            instance_stem,
            connections,
        } => {
            // Always substitute upper_expr (it's evaluated in the outer scope).
            let upper_expr = subst_expr_loop_var(upper_expr, loop_var, k);
            // Inner loop_var shadows ours — leave inner body untouched.
            let (parameter_assignments, connections) = if inner_var == loop_var {
                (parameter_assignments, connections)
            } else {
                (
                    parameter_assignments
                        .into_iter()
                        .map(|(n, e)| (n, subst_expr_loop_var(e, loop_var, k)))
                        .collect(),
                    subst_port_connections(&connections, loop_var, k),
                )
            };
            CstModuleItem::GenerateFor {
                loop_var: inner_var,
                upper_expr,
                module_name,
                parameter_assignments,
                instance_stem,
                connections,
            }
        }
        CstModuleItem::GenerateForAssigns {
            loop_var: inner_var,
            upper_expr,
            assigns,
        } => {
            let upper_expr = subst_expr_loop_var(upper_expr, loop_var, k);
            let assigns = if inner_var == loop_var {
                assigns
            } else {
                assigns
                    .into_iter()
                    .map(|(t, e, r)| (
                        subst_target_loop_var(t, loop_var, k),
                        subst_expr_loop_var(e, loop_var, k),
                        r,
                    ))
                    .collect()
            };
            CstModuleItem::GenerateForAssigns {
                loop_var: inner_var,
                upper_expr,
                assigns,
            }
        }
        CstModuleItem::GenerateForBody {
            loop_var: inner_var,
            upper_expr,
            body,
        } => {
            let upper_expr = subst_expr_loop_var(upper_expr, loop_var, k);
            let body = if inner_var == loop_var {
                body
            } else {
                body.into_iter()
                    .map(|i| subst_module_item_loop_var(i, loop_var, k))
                    .collect()
            };
            CstModuleItem::GenerateForBody {
                loop_var: inner_var,
                upper_expr,
                body,
            }
        }
        CstModuleItem::GenerateIf {
            cond,
            then_body,
            else_body,
        } => CstModuleItem::GenerateIf {
            cond: subst_expr_loop_var(cond, loop_var, k),
            then_body: then_body
                .into_iter()
                .map(|i| subst_module_item_loop_var(i, loop_var, k))
                .collect(),
            else_body: else_body
                .into_iter()
                .map(|i| subst_module_item_loop_var(i, loop_var, k))
                .collect(),
        },
        CstModuleItem::GenerateCase {
            scrutinee,
            arms,
            default,
        } => CstModuleItem::GenerateCase {
            scrutinee: subst_expr_loop_var(scrutinee, loop_var, k),
            arms: arms
                .into_iter()
                .map(|(v, body)| (
                    subst_expr_loop_var(v, loop_var, k),
                    body.into_iter()
                        .map(|i| subst_module_item_loop_var(i, loop_var, k))
                        .collect(),
                ))
                .collect(),
            default: default
                .into_iter()
                .map(|i| subst_module_item_loop_var(i, loop_var, k))
                .collect(),
        },
        other => other, // NetDecl, Always, Initial, LocalParam, etc. — pass through.
    }
}


/// Substitute multiple identifiers in an [`Expr`]. Walks the expression and
/// replaces every `Ident(name)` whose name matches a `(formal, actual)` pair
/// in `subs` with the actual `Expr`. Used to inline function calls.
pub(crate) fn subst_expr_multi(e: Expr, subs: &[(String, Expr)]) -> Expr {
    match e {
        Expr::Ident(name) => {
            if let Some((_, actual)) = subs.iter().find(|(f, _)| f == &name) {
                actual.clone()
            } else {
                Expr::Ident(name)
            }
        }
        Expr::Number(n) => Expr::Number(n),
        Expr::Binary { op, left, right } => Expr::Binary {
            op,
            left: Box::new(subst_expr_multi(*left, subs)),
            right: Box::new(subst_expr_multi(*right, subs)),
        },
        Expr::Unary { op, operand } => Expr::Unary {
            op,
            operand: Box::new(subst_expr_multi(*operand, subs)),
        },
        Expr::Ternary { cond, then_expr, else_expr } => Expr::Ternary {
            cond: Box::new(subst_expr_multi(*cond, subs)),
            then_expr: Box::new(subst_expr_multi(*then_expr, subs)),
            else_expr: Box::new(subst_expr_multi(*else_expr, subs)),
        },
        Expr::Concat(exprs) => Expr::Concat(
            exprs.into_iter().map(|e| subst_expr_multi(e, subs)).collect(),
        ),
        Expr::Index { base, msb, lsb } => Expr::Index {
            base: Box::new(subst_expr_multi(*base, subs)),
            msb: Box::new(subst_expr_multi(*msb, subs)),
            lsb: lsb.map(|x| Box::new(subst_expr_multi(*x, subs))),
        },
        Expr::Clog2(a) => Expr::Clog2(Box::new(subst_expr_multi(*a, subs))),
        Expr::Signed(a) => Expr::Signed(Box::new(subst_expr_multi(*a, subs))),
    }
}

/// Substitute identifiers in an [`AssignTarget`] using the same rules as
/// [`subst_expr_multi`]. When a whole-target identifier matches a formal,
/// the substitution is dropped on the floor (we can't substitute a plain
/// reg name with an arbitrary Expr in LHS position) — callers should not
/// rely on substituting LHS regs for task inlining.
fn subst_target_multi(t: AssignTarget, subs: &[(String, Expr)]) -> AssignTarget {
    match t {
        AssignTarget::Whole(name) => AssignTarget::Whole(name),
        AssignTarget::BitSelect { reg, index } => AssignTarget::BitSelect {
            reg,
            index: subst_expr_multi(index, subs),
        },
        AssignTarget::PartSelect { reg, msb, lsb } => AssignTarget::PartSelect {
            reg,
            msb: subst_expr_multi(msb, subs),
            lsb: subst_expr_multi(lsb, subs),
        },
        AssignTarget::Concat(parts) => AssignTarget::Concat(
            parts.into_iter().map(|p| subst_target_multi(p, subs)).collect(),
        ),
    }
}

/// Substitute identifiers in a [`CstStmt`]. Used to inline a task body at a
/// call site. Walks RHS expressions and bit/part-select indices on LHS.
pub(crate) fn subst_stmt_multi(s: CstStmt, subs: &[(String, Expr)]) -> CstStmt {
    match s {
        CstStmt::BlockingAssign { target, rhs } => CstStmt::BlockingAssign {
            target: subst_target_multi(target, subs),
            rhs: subst_expr_multi(rhs, subs),
        },
        CstStmt::NonBlockingAssign { target, rhs } => CstStmt::NonBlockingAssign {
            target: subst_target_multi(target, subs),
            rhs: subst_expr_multi(rhs, subs),
        },
        CstStmt::Delay(d) => CstStmt::Delay(d),
        CstStmt::SystemTask { name, args } => CstStmt::SystemTask {
            name,
            args: args.into_iter().map(|e| subst_expr_multi(e, subs)).collect(),
        },
        other => other, // IfElse / Case / For — would need recursive walks; OK for now.
    }
}
