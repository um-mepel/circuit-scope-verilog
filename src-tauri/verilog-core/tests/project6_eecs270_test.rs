//! EECS 270 Project 6 (Traffic light + B4to7SEG) — regression against coursework RTL.
//!
//! Reference behavior (Icarus `Project6.vcd`): when `timer == 10` and not blanking,
//! `digits[10] == 7'b1000000` drives LSD; top `HEX6` must match `TimerLSD`.
//! Circuit Scope previously showed `HEX6` / `LSD` as wrong (e.g. `b0`) while `digits[*` stayed `x`.

use std::collections::HashMap;
use std::path::PathBuf;

use verilog_core::{
    build_ir_for_file, generate_vcd, optimize_project, IrProject, SimConfig,
};

fn p6_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/codegen_fixtures/eecs270_p6")
}

fn run_testbench6_vcd(num_cycles: usize) -> String {
    let dir = p6_dir();
    let mut project = IrProject {
        modules: vec![],
        diagnostics: vec![],
        source_map: verilog_core::source_map::SourceMap::new(),
    };
    for name in ["TestBench6.v", "TLC.v", "B4to7SEG.v"] {
        let p = dir.join(name);
        let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let mut m = build_ir_for_file(p.to_string_lossy(), &src);
        project.modules.append(&mut m.modules);
        project.diagnostics.extend(m.diagnostics);
    }
    let errors: Vec<_> = project
        .diagnostics
        .iter()
        .filter(|d| matches!(d.severity, verilog_core::Severity::Error))
        .collect();
    assert!(
        errors.is_empty(),
        "compile errors: {:?}",
        errors.iter().map(|e| e.format_line()).collect::<Vec<_>>()
    );
    optimize_project(&mut project);
    let config = SimConfig {
        top_module: "TestBench6".into(),
        num_cycles,
        timescale: "1s".into(),
        timescale_precision: "100ms".into(),
        clock_half_period: 5,
        ..Default::default()
    };
    generate_vcd(&project, &config).expect("generate_vcd")
}

/// Map hierarchical leaf name (last word before `[` or `$end`) -> VCD identifier token.
fn vcd_var_ids_in_conv_scope(vcd: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_conv = false;
    for line in vcd.lines() {
        let t = line.trim();
        if t == "$scope module conv $end" {
            in_conv = true;
            continue;
        }
        if in_conv && t.starts_with("$upscope") {
            break;
        }
        if !in_conv || !t.starts_with("$var ") {
            continue;
        }
        let parts: Vec<&str> = t.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let id = parts[3].to_string();
        let mut name = parts[4];
        if name == "[" {
            continue;
        }
        if let Some(idx) = name.find('[') {
            name = &name[..idx];
        }
        out.insert(name.to_string(), id);
    }
    out
}

fn vcd_dumpvars_snapshot(vcd: &str) -> HashMap<String, String> {
    let start = vcd.find("$dumpvars").expect("$dumpvars");
    let rest = &vcd[start + "$dumpvars".len()..];
    let end = rest.find("$end").expect("dumpvars $end");
    let body = &rest[..end];
    let mut m = HashMap::new();
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest_b) = t.strip_prefix('b') {
            if let Some((bits, id)) = rest_b.split_once(' ') {
                m.insert(id.trim().to_string(), bits.to_string());
            }
            continue;
        }
        let chars: Vec<char> = t.chars().collect();
        if chars.len() >= 2 {
            let v = chars[0];
            if matches!(v, '0' | '1' | 'x' | 'X' | 'z' | 'Z') {
                let id: String = chars[1..].iter().collect();
                m.insert(id, v.to_string());
            }
        }
    }
    m
}

fn vcd_var_id_for_top_scope_name(vcd: &str, name: &str) -> Option<String> {
    let mut in_top = false;
    for line in vcd.lines() {
        let t = line.trim();
        if t == "$scope module TestBench6 $end" {
            in_top = true;
            continue;
        }
        if in_top && t.starts_with("$scope module ") && !t.contains("TestBench6") {
            break;
        }
        if !in_top || !t.starts_with("$var ") {
            continue;
        }
        let parts: Vec<&str> = t.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let mut leaf = parts[4];
        if let Some(i) = leaf.find('[') {
            leaf = &leaf[..i];
        }
        if leaf == name {
            return Some(parts[3].to_string());
        }
    }
    None
}

#[test]
fn eecs270_p6_b4_lookup_timer10_lsd_glyph_after_dumpvars() {
    let vcd = run_testbench6_vcd(24);
    let conv = vcd_var_ids_in_conv_scope(&vcd);
    let n_id = conv.get("N").expect("conv.N");
    let blank_id = conv.get("Blank").expect("conv.Blank");
    let lsd_id = conv.get("LSD").expect("conv.LSD");
    let snap = vcd_dumpvars_snapshot(&vcd);
    let n = snap.get(n_id).expect("N value");
    let blank = snap.get(blank_id).expect("Blank value");
    let lsd = snap.get(lsd_id).expect("LSD value");
    assert_eq!(n, "1010", "initial timer should be 10 (4'b1010); snap={snap:?}");
    assert_eq!(blank, "0", "Blank = (timer==0); snap={snap:?}");
    assert_eq!(
        lsd, "1000000",
        "digits[10] is 7'b1000000 for least-significant '0' glyph (matches Icarus); got lsd={lsd} full snap for conv ids: N={n:?} Blank={blank:?}"
    );
}

#[test]
fn eecs270_p6_hex6_must_match_timer_lsd_in_dumpvars() {
    let vcd = run_testbench6_vcd(24);
    let snap = vcd_dumpvars_snapshot(&vcd);
    let hex6_id = vcd_var_id_for_top_scope_name(&vcd, "HEX6").expect("HEX6 var");
    let timer_lsd_id = vcd_var_ids_in_conv_scope(&vcd)
        .remove("LSD")
        .expect("TimerLSD net in conv");
    let hex6 = snap.get(&hex6_id).expect("HEX6 value");
    let tlsd = snap.get(&timer_lsd_id).expect("TimerLSD / conv.LSD");
    assert_eq!(
        hex6, tlsd,
        "TestBench6 wires HEX6 to TimerLSD; VCD must match (replicates p6 HEX6 vs HEX7 issue). hex6={hex6} tlsd={tlsd}"
    );
}
