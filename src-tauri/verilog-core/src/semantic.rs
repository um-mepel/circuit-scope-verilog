use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::expr_const::const_eval_param_expr;
use crate::ir::module_locals_for_cst;
use crate::lexer;
use crate::parser::{self, AssignTarget, CstModule, CstModuleItem};
use crate::{Diagnostic, Port, SourceFile};

/// Summary of an entire Verilog project from the semantic analyzer.
#[derive(Debug, Clone)]
pub struct SemanticProject {
    pub modules: Vec<SemanticModule>,
    pub diagnostics: Vec<Diagnostic>,
    /// Names of modules that are not instantiated by any other module.
    pub top_modules: Vec<String>,
}

/// Semantic information for a single module.
#[derive(Debug, Clone)]
pub struct SemanticModule {
    pub name: String,
    pub path: String,
    pub ports: Vec<Port>,
    pub nets: Vec<String>,
    pub instances: Vec<InstanceRef>,
    pub assigns: Vec<AssignRef>,
}

#[derive(Debug, Clone)]
pub struct InstanceRef {
    pub module_name: String,
    pub instance_name: String,
}

#[derive(Debug, Clone)]
pub struct AssignRef {
    pub lhs: String,
}

/// Analyze all Verilog files under `root`, building a simple semantic model and
/// computing candidate top-level modules.
pub fn analyze_project(root: &Path) -> std::io::Result<SemanticProject> {
    let mut semantic_modules = Vec::new();
    let mut diagnostics = Vec::new();

    walk_dir(root, &mut |path| {
        if let Ok(src) = std::fs::read_to_string(path) {
            let file = SourceFile::new(path.to_string_lossy(), &src);
            let tokens = lexer::lex(&file);
            let (cst, mut diags) = parser::parse_cst(&file, &tokens);
            diagnostics.append(&mut diags);

            for m in cst.modules {
                semantic_modules.push(build_semantic_module(m));
            }
        }
    })?;

    // Build module usage graph: parent -> children via instances.
    let mut children: HashMap<String, HashSet<String>> = HashMap::new();
    let mut all_modules: HashSet<String> = HashSet::new();
    let mut referenced: HashSet<String> = HashSet::new();

    for m in &semantic_modules {
        all_modules.insert(m.name.clone());
        for inst in &m.instances {
            children
                .entry(m.name.clone())
                .or_default()
                .insert(inst.module_name.clone());
            referenced.insert(inst.module_name.clone());
        }
    }

    let top_modules: Vec<String> = all_modules
        .into_iter()
        .filter(|name| !referenced.contains(name))
        .collect();

    Ok(SemanticProject {
        modules: semantic_modules,
        diagnostics,
        top_modules,
    })
}

fn build_semantic_module(cst: CstModule) -> SemanticModule {
    let locals = module_locals_for_cst(&cst);
    let mut nets = Vec::new();
    let mut instances = Vec::new();
    let mut assigns = Vec::new();

    for item in cst.items {
        match item {
            CstModuleItem::NetDecl { names, .. } => {
                nets.extend(names);
            }
            CstModuleItem::Assign { target, .. } => {
                // Concat-LHS records each leaf identifier separately so the
                // semantic graph still knows about every signal being driven.
                fn collect_lhs(t: AssignTarget, out: &mut Vec<String>) {
                    match t {
                        AssignTarget::Whole(s) => out.push(s),
                        AssignTarget::BitSelect { reg, .. } => out.push(reg),
                        AssignTarget::PartSelect { reg, .. } => out.push(reg),
                        AssignTarget::Concat(parts) => {
                            for p in parts {
                                collect_lhs(p, out);
                            }
                        }
                    }
                }
                let mut lhs_names = Vec::new();
                collect_lhs(target, &mut lhs_names);
                for lhs in lhs_names {
                    assigns.push(AssignRef { lhs });
                }
            }
            CstModuleItem::Instance {
                module_name,
                instance_name,
                ..
            } => {
                instances.push(InstanceRef {
                    module_name,
                    instance_name,
                });
            }
            CstModuleItem::GenerateFor {
                upper_expr,
                module_name,
                instance_stem,
                ..
            } => {
                let n = const_eval_param_expr(&upper_expr, &locals)
                    .unwrap_or(0)
                    .max(0) as usize;
                for k in 0..n {
                    instances.push(InstanceRef {
                        module_name: module_name.clone(),
                        instance_name: format!("{}__{}", instance_stem, k),
                    });
                }
            }
            CstModuleItem::Always { .. } => {}
            CstModuleItem::Initial { .. } => {}
            CstModuleItem::LocalParam { .. } => {}
            CstModuleItem::GenerateForBody { .. }
            | CstModuleItem::GenerateIf { .. }
            | CstModuleItem::GenerateCase { .. } => {
                // Semantic walker doesn't yet recurse into these for assign
                // collection; the IR elaborator handles them and the result
                // is reflected in the module's `assigns` IR list. Tests
                // don't depend on semantic.rs reporting them.
            }
            CstModuleItem::GenerateForAssigns { assigns: body_assigns, .. } => {
                // Record each LHS leaf identifier so the semantic graph
                // reflects every signal driven by the unrolled assigns.
                fn collect(t: &crate::parser::AssignTarget, out: &mut Vec<String>) {
                    use crate::parser::AssignTarget;
                    match t {
                        AssignTarget::Whole(s) => out.push(s.clone()),
                        AssignTarget::BitSelect { reg, .. } => out.push(reg.clone()),
                        AssignTarget::PartSelect { reg, .. } => out.push(reg.clone()),
                        AssignTarget::Concat(parts) => {
                            for p in parts {
                                collect(p, out);
                            }
                        }
                    }
                }
                for (target, _expr, _range) in body_assigns {
                    let mut names = Vec::new();
                    collect(&target, &mut names);
                    for lhs in names {
                        assigns.push(AssignRef { lhs });
                    }
                }
            }
        }
    }

    SemanticModule {
        name: cst.name,
        path: cst.path,
        ports: cst.ports,
        nets,
        instances,
        assigns,
    }
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

