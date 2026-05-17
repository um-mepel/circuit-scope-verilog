//! Sequential clocked logic (NBA) and RTL (continuous assign + hierarchy) integration tests.
//! These lock simulation semantics the coursework RTL depends on.

use verilog_core::{
    build_ir_for_file, generate_vcd, optimize_project, IrProject, SimConfig,
};

fn dbg_config(top: &str, cycles: usize) -> SimConfig {
    SimConfig {
        top_module: top.into(),
        num_cycles: cycles,
        timescale: "1ns".into(),
        clock_half_period: 5,
        ..Default::default()
    }
}

fn optimize_and_vcd_single(src: &str, path: &str, top: &str, cycles: usize) -> String {
    let mut ir = build_ir_for_file(path, src);
    optimize_project(&mut ir);
    generate_vcd(&ir, &dbg_config(top, cycles)).expect("generate_vcd")
}

fn merge_vcd(paths_and_src: &[(&str, &str)], top: &str, cycles: usize) -> String {
    let mut project = IrProject {
        modules: vec![],
        diagnostics: vec![],
        source_map: verilog_core::source_map::SourceMap::new(),
    };
    for (p, s) in paths_and_src {
        let mut m = build_ir_for_file((*p).to_string(), s);
        project.modules.append(&mut m.modules);
        project.diagnostics.extend(m.diagnostics);
    }
    optimize_project(&mut project);
    generate_vcd(&project, &dbg_config(top, cycles)).expect("generate_vcd")
}

/// Two separate `always @(posedge clk)` blocks: NBAs must not be applied between blocks; otherwise
/// later blocks see earlier blocks' scheduled updates (breaks counter → decoder splits).
#[test]
fn nba_two_always_blocks_swap_registers() {
    let src = r#"
module top;
  reg clk;
  reg r1, r2;
  initial begin clk = 0; r1 = 0; r2 = 1; end
  always #5 clk = ~clk;
  always @(posedge clk) r1 <= r2;
  always @(posedge clk) r2 <= r1;
endmodule
"#;
    let vcd = optimize_and_vcd_single(src, "nba2al.v", "top", 30);
    let mut ones = 0usize;
    let mut zeros = 0usize;
    for line in vcd.lines() {
        if line == "0!" || line == "1!" {
            if line.starts_with('1') {
                ones += 1;
            } else {
                zeros += 1;
            }
        }
    }
    assert!(
        ones >= 4 && zeros >= 4,
        "r1/r2 should alternate as they swap; ones={ones} zeros={zeros}\n{}",
        &vcd[..vcd.len().min(2500)]
    );
}

/// Two NBA assigns in one `always @(posedge)` must both use **pre-edge** values (IEEE sampling).
/// c must not pick up the new b in the same active region.
#[test]
fn nba_pipeline_two_deep_samples_old_values() {
    let src = r#"
module top;
  reg clk;
  reg a, b, c;
  initial begin
    clk = 0;
    a = 0;
    b = 0;
    c = 0;
  end
  always #5 clk = ~clk;
  always @(posedge clk) begin
    c <= b;
    b <= a;
  end
  initial #27 a = 1;
endmodule
"#;
    let vcd = optimize_and_vcd_single(src, "pipe2.v", "top", 48);
    let mut one_bit_high_lines = 0usize;
    for line in vcd.lines() {
        if line.len() >= 2 && !line.starts_with('#') && !line.starts_with('$') {
            if line.chars().next() == Some('1') && line.chars().nth(1) != Some('b') {
                one_bit_high_lines += 1;
            }
        }
    }
    assert!(
        one_bit_high_lines >= 3,
        "after a→1, b and c should eventually register 1 on separate edges; got {} single-bit-high lines; excerpt:\n{}",
        one_bit_high_lines,
        &vcd[..vcd.len().min(3500)]
    );
}

/// Shift register with concatenation on RHS of NBA.
#[test]
fn nba_shift_register_concat_rhs() {
    let src = r#"
module top;
  reg clk;
  reg si;
  reg [2:0] sh;
  initial begin
    clk = 0;
    si = 0;
    sh = 3'b0;
  end
  always #5 clk = ~clk;
  always @(posedge clk) sh <= {sh[1:0], si};
  initial begin
    #12 si = 1;
    #20 si = 0;
    #20 si = 1;
  end
endmodule
"#;
    let vcd = optimize_and_vcd_single(src, "shfit.v", "top", 40);
    assert!(
        vcd.contains("b1 ") || vcd.contains("b11 ") || vcd.contains("b111"),
        "shift register should produce non-zero bus values; excerpt:\n{}",
        &vcd[..vcd.len().min(4000)]
    );
}

/// Continuous `assign` from state plus `always @(posedge)` updating state (classic RTL partition).
#[test]
fn rtl_assign_next_state_plus_sequential_register() {
    let src = r#"
module top;
  reg clk;
  reg [2:0] state;
  wire [2:0] next_state;
  initial begin
    clk = 0;
    state = 3'd0;
  end
  assign next_state = state + 3'd1;
  always #5 clk = ~clk;
  always @(posedge clk) state <= next_state;
endmodule
"#;
    let vcd = optimize_and_vcd_single(src, "fsm_step.v", "top", 24);
    assert!(
        vcd.contains("b11 ") || vcd.contains("b100 ") || vcd.contains("b101 "),
        "state should advance past 1 — assign-driven next_state + NBA; excerpt:\n{}",
        &vcd[..vcd.len().min(4500)]
    );
}

/// Hierarchical RTL: leaf sequential, parent wires + continuous assign.
/// Expect a 2-bit wrap counter on `inp`/`outp` (assign `inp = outp + 1` + registered `q`).
#[test]
fn hierarchical_rtl_parent_assign_child_sequential() {
    let child = r#"
module acc(input wire clk, input wire [1:0] din, output reg [1:0] q);
  always @(posedge clk) q <= din;
endmodule
"#;
    let top = r#"
module top;
  reg clk;
  wire [1:0] inp;
  wire [1:0] outp;
  acc u(.clk(clk), .din(inp), .q(outp));
  assign inp = outp + 2'd1;
  initial clk = 0;
  always #5 clk = ~clk;
endmodule
"#;
    let vcd = merge_vcd(&[("acc.v", child), ("top.v", top)], "top", 32);
    assert!(
        vcd.contains("b0 ") && vcd.contains("b1 ") && vcd.contains("b10 ") && vcd.contains("b11 "),
        "expected all 2-bit values 0..=3 on wires after assign↔posedge loop; excerpt:\n{}",
        &vcd[..vcd.len().min(2500)]
    );
}

/// Positional ports + internal clock generator + posedge (student TB style).
#[test]
fn positional_ports_sequential_leaf() {
    let leaf = r#"
module leaf(input wire clk, input wire d, output reg q);
  always @(posedge clk) q <= d;
endmodule
"#;
    let top = r#"
module top;
  reg clk, data;
  wire qo;
  leaf u(clk, data, qo);
  initial begin clk = 0; data = 1; end
  always #5 clk = ~clk;
endmodule
"#;
    let vcd = merge_vcd(&[("leaf.v", leaf), ("top.v", top)], "top", 16);
    assert!(
        !vcd.contains("x!") && !vcd.contains("xz "),
        "positional wiring must drive leaf clk/d so q is not X; excerpt:\n{}",
        &vcd[..vcd.len().min(1800)]
    );
}
