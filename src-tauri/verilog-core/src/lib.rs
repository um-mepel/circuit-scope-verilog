//! **`verilog-core` — IEEE 1364 Verilog first.**
//!
//! The language design target is **Verilog** (IEEE Std 1364): `module`, `wire` / `reg`, `assign`,
//! `always`, `initial`, delays, and hierarchical instances. This is **not** a SystemVerilog
//! compiler: classes, interfaces, packages, assertions, and most SV-only syntax are out of scope.
//!
//! A few tokens found in mixed classroom or tool output (notably `logic` as a net type) are
//! accepted so sources are not misparsed; portable coursework should still prefer `wire` / `reg`.
//!
//! **`.v` and `.sv` files** are discovered as *Verilog RTL* sources—the extension is convention
//! only; both use the same parser and subset.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod arith;
mod csverilog_pipeline;
pub mod codegen;
pub mod delay_rational;
mod expr_const;
mod ir;
mod timescale_util;
pub mod lexer;
pub mod optimizer;
mod parser;
mod semantic;
pub mod sim_session;
pub mod source_map;
pub mod trace;

pub use crate::ir::{
    build_ir_for_file, build_ir_for_path_bufs, build_ir_for_root, elaborate_parameterized_modules,
    resolve_instance_port_connections, sum_initial_delay_literals_for_source_file, IrAlways,
    IrAssign, IrBinOp, IrCaseArm, IrEdgeKind, IrExpr, IrInitial, IrInstance, IrModule, IrNet,
    IrPortConn, IrProject, IrSensEntry, IrSensitivity, IrStmt, IrUnaryOp, StmtBlock,
};
pub use crate::source_map::{FileId, LineCol, SourceMap, Span, SpanInfo, SYNTHETIC_FILE};
pub use crate::lexer::{Token, TokenKind};
pub use crate::optimizer::{optimize_module, optimize_project, optimize_module_with_metrics, OptimizeMetrics};
pub use crate::codegen::{
    generate_vcd, generate_vcd_with_trace, new_stepping_simulator, SimConfig, StepMode,
    StepOutcome, VcdRunMeta,
};
pub use crate::sim_session::{
    DriverQueryResult, SessionId, SessionRegistry, SimRunner, SimSession,
};
pub use crate::trace::{
    driver_latest_at_or_before, trace_at, trace_latest_at_or_before, BranchChoice, DriverEvent,
    TraceEntry,
};
pub use crate::delay_rational::DelayRational;
pub use crate::csverilog_pipeline::{
    num_cycles_from_initial_delay_sum, run_csverilog_pipeline, scan_timescale_project,
    sum_initial_delays_for_source_files, CsVerilogOptions, TimescaleScan,
};
pub use crate::timescale_util::{
    clock_half_period_fine_ticks, num_cycles_from_initial_delay_sum_fine,
    unit_per_precision_ratio,
};
pub use crate::semantic::{
    analyze_project, AssignRef, InstanceRef, SemanticModule, SemanticProject,
};

/// `verilog-core` crate version at compile time (matches VCD `$version` / `$comment` metadata).
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment variable set by Circuit Scope's integrated shell so **`csverilog`** matches **File → Generate VCD**.
pub const CIRCUIT_SCOPE_PROJECT_ROOT_VAR: &str = "CIRCUIT_SCOPE_PROJECT_ROOT";

/// Scan root for **`csverilog`** default (non-`--explicit`) mode:
/// [`CIRCUIT_SCOPE_PROJECT_ROOT_VAR`] if set and names an existing directory, otherwise `cwd`.
pub fn circuit_scope_project_root_for_scan(cwd: &Path) -> PathBuf {
    std::env::var(CIRCUIT_SCOPE_PROJECT_ROOT_VAR)
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// One-shot: walk `root` for Verilog sources (`.v` / `.sv`), optimise, simulate, return VCD (uses [`build_ir_for_root`], not [`crate::list_verilog_source_paths`]).
///
/// The **Circuit Scope** menu and **`csverilog`** CLI use [`run_csverilog_pipeline`] instead. Both are valid; file discovery differs slightly (same skip rules).
pub fn simulate_to_vcd(root: &Path) -> Result<String, String> {
    simulate_to_vcd_with(root, None, None, None)
}

/// Like [`simulate_to_vcd`] but lets the caller override the top module
/// name and number of cycles, and optionally name the output file for VCD header metadata.
pub fn simulate_to_vcd_with(
    root: &Path,
    top_override: Option<&str>,
    cycles: Option<usize>,
    output_filename_hint: Option<&str>,
) -> Result<String, String> {
    let mut project = build_ir_for_root(root).map_err(|e| e.to_string())?;
    optimize_project(&mut project);

    let top_name = match top_override {
        Some(name) => name.to_string(),
        None => find_top_module(&project)?,
    };

    let out_name = output_filename_hint.unwrap_or("circuit_scope.vcd");
    let output_vcd_path = root.join(out_name);
    let output_vcd_path = output_vcd_path.to_string_lossy().into_owned();

    let top_mod_path = project
        .modules
        .iter()
        .find(|m| m.name == top_name)
        .map(|m| PathBuf::from(&m.path));

    let vcd_meta = VcdRunMeta {
        top_source_file: top_mod_path
            .as_ref()
            .and_then(|p| p.to_str().map(|s| s.to_string())),
        command_line: Some(format!(
            "simulate_to_vcd_with(root=\"{}\", top_override={:?}, cycles={:?}, output_filename_hint={:?})",
            root.display(),
            top_override,
            cycles,
            output_filename_hint
        )),
        output_vcd_path: Some(output_vcd_path),
        working_directory: std::env::current_dir()
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok()),
        ..Default::default()
    };

    let verilog_paths = list_verilog_source_paths(root).map_err(|e| e.to_string())?;
    let ts_decl = crate::csverilog_pipeline::scan_timescale_project(&verilog_paths)?;
    let delay_files: Vec<PathBuf> = if ts_decl.timescale_source_files.is_empty() {
        top_mod_path.clone().into_iter().collect()
    } else {
        ts_decl.timescale_source_files.clone()
    };
    let delay_sum =
        crate::csverilog_pipeline::sum_initial_delays_for_source_files(&project, &delay_files);
    let clock_half_period = 5usize;
    let clock_half_period_is_explicit = false;
    let k = crate::timescale_util::unit_per_precision_ratio(
        &ts_decl.time_unit,
        &ts_decl.time_precision,
    );
    let h_fine = crate::timescale_util::clock_half_period_fine_ticks(
        clock_half_period,
        k,
        clock_half_period_is_explicit,
        &ts_decl.time_unit,
    );
    let initial_delay_sum_units = if delay_sum > 0 { Some(delay_sum) } else { None };
    let num_cycles = cycles.unwrap_or_else(|| {
        crate::timescale_util::num_cycles_from_initial_delay_sum_fine(delay_sum, k, h_fine)
    });

    let config = SimConfig {
        top_module: top_name,
        num_cycles,
        timescale: ts_decl.time_unit.clone(),
        timescale_precision: ts_decl.time_precision.clone(),
        clock_half_period,
        clock_half_period_is_explicit,
        initial_delay_sum_units,
        vcd_meta: Some(vcd_meta),
        ..Default::default()
    };

    generate_vcd(&project, &config)
}

/// Picks a simulation top when the caller did not pass `--top`: uninstantiated modules,
/// preferring testbench-shaped names/paths (matches **File → Generate VCD** and `simulate_to_vcd`).
pub fn find_top_module(project: &ir::IrProject) -> Result<String, String> {
    /// Aligned with [`list_verilog_source_paths`] `tb_path_rank` filename heuristics.
    fn source_file_looks_like_testbench(path: &str) -> bool {
        std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| {
                let n = n.to_lowercase();
                n.contains("testbench") || n.ends_with("_tb.v") || n.starts_with("tb_")
            })
            .unwrap_or(false)
    }

    // Collect all module names that are instantiated by another module.
    let mut instantiated: std::collections::HashSet<&str> =
        std::collections::HashSet::new();
    for m in &project.modules {
        for inst in &m.instances {
            instantiated.insert(inst.module_name.as_str());
        }
    }
    // The top module is the one nobody instantiates.
    let mut tops: Vec<&ir::IrModule> = project
        .modules
        .iter()
        .filter(|m| !instantiated.contains(m.name.as_str()))
        .collect();

    match tops.len() {
        0 => Err("no modules found in project".into()),
        1 => Ok(tops[0].name.clone()),
        _ => {
            // Multiple roots happen when `build_ir_for_root` pulls in many unrelated
            // `.v` files (e.g. compiler test fixtures). Prefer an obvious testbench.
            let mut tb_like: Vec<&ir::IrModule> = tops
                .iter()
                .copied()
                .filter(|m| {
                    let n = m.name.to_lowercase();
                    n.contains("testbench")
                        || n.contains("test_bench")
                        || n.ends_with("_tb")
                        || n.starts_with("tb_")
                })
                .collect();
            if tb_like.len() == 1 {
                return Ok(tb_like[0].name.clone());
            }
            if tb_like.len() > 1 {
                tb_like.sort_by(|a, b| a.name.cmp(&b.name));
                return Ok(tb_like.last().unwrap().name.clone());
            }

            let mut path_tb: Vec<&ir::IrModule> = tops
                .iter()
                .copied()
                .filter(|m| source_file_looks_like_testbench(m.path.as_str()))
                .collect();
            if path_tb.len() == 1 {
                return Ok(path_tb[0].name.clone());
            }
            if path_tb.len() > 1 {
                path_tb.sort_by(|a, b| a.name.cmp(&b.name));
                return Ok(path_tb.last().unwrap().name.clone());
            }

            // Favor the usual stimulus wrapper (initial/always generators).
            let with_initial: Vec<&ir::IrModule> = tops
                .iter()
                .copied()
                .filter(|m| !m.initial_blocks.is_empty())
                .collect();
            if with_initial.len() == 1 {
                return Ok(with_initial[0].name.clone());
            }

            let with_inst: Vec<&ir::IrModule> = tops
                .iter()
                .copied()
                .filter(|m| !m.instances.is_empty())
                .collect();
            if with_inst.len() == 1 {
                return Ok(with_inst[0].name.clone());
            }

            tops.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(tops.last().unwrap().name.clone())
        }
    }
}

// === Core data types ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub content: String,
}

impl SourceFile {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub message: String,
    pub severity: Severity,
    pub line: usize,
    pub column: usize,
    /// Source path as passed to [`parse_file`] / [`build_ir_for_file`] (e.g. absolute file path).
    #[serde(default)]
    pub path: String,
}

impl Diagnostic {
    /// Single-line message: `path:line:column: severity: message`
    pub fn format_line(&self) -> String {
        let path = if self.path.is_empty() {
            "<unknown>"
        } else {
            self.path.as_str()
        };
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        format!(
            "{}:{}:{}: {}: {}",
            path, self.line, self.column, sev, self.message
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Port {
    pub direction: Option<String>, // input / output / inout
    pub name: String,
    #[serde(default = "default_width")]
    pub width: usize,
}

fn default_width() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module {
    pub name: String,
    pub ports: Vec<Port>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub modules: Vec<Module>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndex {
    pub modules: Vec<ModuleEntry>,
}

// === Public API ===

pub fn parse_file(path: impl Into<String>, content: &str) -> ParseResult {
    let file = SourceFile::new(path, content);
    let tokens = lexer::lex(&file);
    parser::parse_file(&file, &tokens)
}

/// All `.v` / `.sv` paths under `root` using the same rules as indexing (skips `target`, `tests`, etc.).
pub fn list_verilog_source_paths(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    walk_dir(root, &mut |p| paths.push(p.to_path_buf()))?;
    paths.sort();
    paths.dedup();
    paths.sort_by(|a, b| tb_path_rank(a).cmp(&tb_path_rank(b)).then_with(|| a.cmp(b)));
    Ok(paths)
}

fn tb_path_rank(p: &Path) -> u8 {
    let name = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if name.contains("testbench") || name.ends_with("_tb.v") || name.starts_with("tb_") {
        0
    } else {
        1
    }
}

pub fn index_project(root: &Path) -> std::io::Result<ProjectIndex> {
    let mut modules = Vec::new();
    walk_dir(root, &mut |path| {
        if let Ok(src) = std::fs::read_to_string(path) {
            let res = parse_file(path.to_string_lossy(), &src);
            for m in res.modules {
                modules.push(ModuleEntry {
                    name: m.name,
                    path: m.path,
                });
            }
        }
    })?;
    Ok(ProjectIndex { modules })
}

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
            // Skip dependency / build trees and Rust `tests/` folders so opening the
            // repo root does not load hundreds of unrelated RTL fixtures as extra tops.
            if matches!(
                name,
                "target"
                    | "node_modules"
                    | ".git"
                    | "dist"
                    | "tests"
                    | "fixtures"
                    | "artifacts"
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

/// Whether `path` is treated as a Verilog source (1364 RTL; `.sv` uses the same parser).
fn is_verilog_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_lowercase().as_str(), "v" | "sv"),
        None => false,
    }
}

#[cfg(test)]
mod circuit_scope_project_root_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn circuit_scope_project_root_for_scan_without_env_is_cwd() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(CIRCUIT_SCOPE_PROJECT_ROOT_VAR);
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            circuit_scope_project_root_for_scan(tmp.path()),
            tmp.path().to_path_buf()
        );
    }

    #[test]
    fn circuit_scope_project_root_for_scan_uses_env_when_dir_exists() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_var(
            CIRCUIT_SCOPE_PROJECT_ROOT_VAR,
            tmp.path().to_string_lossy().as_ref(),
        );
        assert_eq!(
            circuit_scope_project_root_for_scan(&cwd),
            tmp.path().to_path_buf()
        );
        std::env::remove_var(CIRCUIT_SCOPE_PROJECT_ROOT_VAR);
    }
}

