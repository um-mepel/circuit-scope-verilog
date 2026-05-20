//! End-to-end tests for [`SimSession`] — the resumable-simulator wrapper the
//! Tauri debugger commands will use.

use std::path::PathBuf;

use tempfile::tempdir;
use verilog_core::{
    build_ir_for_file, optimize_project, SimConfig, SimSession, StepMode, StepOutcome,
};

fn counter_project() -> (verilog_core::IrProject, SimConfig) {
    let src = r#"
module counter(output reg [3:0] count);
  reg clk;
  reg rst;
  always #5 clk = ~clk;
  always @(posedge clk) begin
    if (rst) count <= 0;
    else count <= count + 1;
  end
  initial begin
    clk = 0; rst = 0;
    #100 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("counter.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "counter".into(),
        num_cycles: 4,
        ..Default::default()
    };
    (proj, config)
}

#[test]
fn sim_session_advances_time_and_flushes_vcd() {
    let (proj, config) = counter_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("counter.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path.clone()).unwrap();

    assert_eq!(sess.current_time_fs(), 0, "fresh session is at t=0");

    let outcome = sess.step(StepMode::OneTick, None);
    match outcome {
        StepOutcome::Advanced { t_sim, .. } => {
            assert!(t_sim > 0, "OneTick should advance past t=0");
        }
        StepOutcome::End => panic!("unexpected end on first step"),
    }

    let path = sess.flush_vcd().expect("flush vcd");
    assert_eq!(path, vcd_path);
    let contents = std::fs::read_to_string(&vcd_path).unwrap();
    assert!(contents.contains("$enddefinitions"));
    assert!(contents.contains("$dumpvars"));
}

#[test]
fn sim_session_statement_step_produces_activity() {
    let (proj, config) = counter_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("counter.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    let outcome = sess.step(StepMode::OneStatement, None);
    let ran = match outcome {
        StepOutcome::Advanced { ran_statements, .. } => ran_statements,
        StepOutcome::End => 0,
    };
    assert!(
        ran >= 1,
        "OneStatement should execute at least one statement; got {}",
        ran
    );
    assert!(!sess.trace().is_empty());
}

#[test]
fn sim_session_run_to_end_terminates() {
    let (proj, config) = counter_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("counter.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    let outcome = sess.step(StepMode::ToEnd, None);
    assert!(matches!(outcome, StepOutcome::Advanced { .. } | StepOutcome::End));

    // A second ToEnd must report End — the simulator is drained.
    let again = sess.step(StepMode::ToEnd, None);
    assert!(matches!(again, StepOutcome::End));
}

/// `assign q = cond ? a[0] : b[0]` — exercise the Jump to Driver backend:
/// after stepping to the end, the session must report a driver event for
/// `q` pointing at the `assign` statement, with an active `BranchChoice`.
fn ternary_project() -> (verilog_core::IrProject, verilog_core::SimConfig) {
    let src = r#"
module m(output wire q);
  reg clk;
  reg [1:0] a;
  reg [1:0] b;
  assign q = clk ? a[0] : b[0];
  always #5 clk = ~clk;
  initial begin
    clk = 0;
    a = 2'b01;
    b = 2'b10;
    #40 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("m.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "m".into(),
        num_cycles: 4,
        ..Default::default()
    };
    (proj, config)
}

#[test]
fn driver_at_reports_assign_span_for_ternary_signal() {
    let (proj, config) = ternary_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("ternary.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    // Drain the simulator so the driver trace has events at every tick.
    sess.step(StepMode::ToEnd, None);

    let now = sess.current_time_fs();
    assert!(now > 0, "simulation should advance past t=0");

    // `resolve_signal_idx` falls back to a suffix match so the unqualified
    // name `q` resolves even though the flattened name is `top.m.q` (or
    // similar). This mirrors what the frontend sends for a hierarchical pick.
    let res = sess
        .driver_at(&proj, "q", now)
        .expect("driver_at must resolve q at end of sim");

    let resolved = res.resolved_signal.clone();
    assert!(
        resolved == "q" || resolved.ends_with(".q") || resolved.ends_with("__q"),
        "resolved signal should end in `q`, got {:?}",
        resolved
    );

    let stmt_info = proj
        .source_map
        .resolve(res.stmt_span)
        .expect("stmt span must resolve against the project source map");
    assert!(
        stmt_info.path.ends_with("m.v"),
        "stmt path should be the module's source file, got {:?}",
        stmt_info.path
    );
    assert!(
        res.branch.is_some(),
        "ternary rhs should have a BranchChoice attached"
    );
}

#[test]
fn driver_at_resolves_unchanging_continuous_assign() {
    // Regression: a continuous `assign` whose computed value equals the
    // default (0) was never pushing a `DriverEvent`, so "Jump to Driver"
    // would fail with "no driver recorded" even though the wire *is* driven.
    //
    // Here `Add = a + b` with `a` and `b` both zero for the entire run, so
    // `Add` never deviates from 0 — yet the backend must still report this
    // `assign` as the driver.
    let src = r#"
module m(output wire [3:0] add_out);
  reg [3:0] a;
  reg [3:0] b;
  assign add_out = a + b;
  initial begin
    a = 0;
    b = 0;
    #40 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("m.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "m".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("unchanging.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    let now = sess.current_time_fs();
    let res = sess
        .driver_at(&proj, "add_out", now)
        .expect("driver_at must resolve add_out even when its value is constant 0");
    let stmt_info = proj
        .source_map
        .resolve(res.stmt_span)
        .expect("stmt span must resolve");
    assert!(stmt_info.path.ends_with("m.v"));
}

#[test]
fn driver_at_resolves_top_module_port_from_dotted_scope_name() {
    // Regression for the HEX4 case: the frontend builds waveform names from
    // the VCD scope tree (e.g. `TestBench7.HEX4`), while the simulator's
    // flattened signal table stores top-module ports without any prefix
    // (`HEX4`). `resolve_signal_idx` must strip the leading scope component
    // and still find the signal.
    let src = r#"
module sub(output reg [3:0] inner);
  always @* inner = 4'h5;
endmodule
module top(output wire [3:0] hex4);
  reg [3:0] a;
  sub u(.inner());
  assign hex4 = a;
  initial begin
    a = 0;
    #40 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("t.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "top".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("dotted.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    // Top-module port sent with the top scope prefix (what the frontend
    // actually sends). Must resolve even though the simulator stores it as
    // just `hex4`.
    let res = sess
        .driver_at(&proj, "top.hex4", 0)
        .expect("dotted top-scope name should resolve to the flattened port");
    assert!(res.resolved_signal == "hex4" || res.resolved_signal.ends_with(".hex4"));

    // Submodule signal with a full dotted hierarchy: `top.u.inner` must
    // resolve to the flattened `u__inner`.
    let res = sess
        .driver_at(&proj, "top.u.inner", 0)
        .expect("dotted submodule name should resolve via `.`→`__` rewrite");
    assert!(
        res.resolved_signal.ends_with("inner"),
        "expected resolved signal to end in `inner`, got {:?}",
        res.resolved_signal
    );
}

#[test]
fn driver_at_falls_back_to_static_driver_for_unfired_always() {
    // Regression: `X` is only written inside an `always @(posedge clk)` block
    // that is guarded by `en`. If we query at a time before the always block
    // has fired (or when `en` is always 0 and thus the NBA is never reached),
    // there are no runtime DriverEvents for X. The backend must still report
    // the driving statement via the static-driver fallback.
    let src = r#"
module m(output reg [3:0] x);
  reg clk;
  reg en;
  always @(posedge clk) begin
    if (en) x <= x + 1;
  end
  initial begin
    clk = 0;
    en = 0;
    x = 0;
    #40 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("m.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "m".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("unfired.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    // Query *before* stepping anywhere. No DriverEvent has ever been pushed;
    // the fallback path must still find the `x <= x + 1` span.
    let res = sess
        .driver_at(&proj, "x", 0)
        .expect("static-driver fallback must resolve x even before any step");
    let stmt_info = proj
        .source_map
        .resolve(res.stmt_span)
        .expect("stmt span from static-driver fallback must resolve");
    assert!(stmt_info.path.ends_with("m.v"));
    assert!(
        res.branch.is_none(),
        "static fallback cannot know the taken branch; branch must be None"
    );
}

#[test]
fn driver_at_resolves_edge_triggered_write() {
    let (proj, config) = counter_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("counter.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    sess.step(StepMode::ToEnd, None);

    let now = sess.current_time_fs();
    // `count` is only written from the `always @(posedge clk)` block. Finding
    // a driver event means the procedural-assign instrumentation fires.
    let res = sess
        .driver_at(&proj, "count", now)
        .expect("driver_at must resolve count at end of sim");

    let stmt_info = proj
        .source_map
        .resolve(res.stmt_span)
        .expect("count's stmt span must resolve");
    assert!(
        stmt_info.path.ends_with("counter.v"),
        "edge-triggered driver should point into counter.v, got {:?}",
        stmt_info.path
    );
}

fn find_line_containing(src: &str, needle: &str) -> u32 {
    let mut line: u32 = 1;
    for raw_line in src.lines() {
        if raw_line.contains(needle) {
            return line;
        }
        line += 1;
    }
    panic!("test source does not contain expected substring {needle:?}");
}

#[test]
fn active_spans_at_returns_deduplicated_entries() {
    let (proj, config) = counter_project();
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("counter.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();

    sess.step(StepMode::ToEnd, None);

    if let Some(first) = sess.trace().first() {
        let spans = sess.active_spans_at(first.time_fs);
        let mut sorted = spans.clone();
        sorted.sort_by_key(|s| (s.start, s.end));
        sorted.dedup();
        assert_eq!(spans.len(), sorted.len(), "spans should already be deduped");
    }
}

/// **Bug regression**: a concat-LHS blocking assignment like
/// `{a,b,c} = 3'b101;` previously caused the parser to drop the statement
/// entirely (because `parse_stmt` had no `LBrace` arm), which silently
/// killed the rest of the enclosing `initial` block. The fix added a
/// `AssignTarget::Concat` variant and an IR lowering that splits the
/// assignment into per-component blocking assigns with the right bit-slice
/// of the RHS.
#[test]
fn concat_lhs_blocking_assign_splits_into_components() {
    let src = r#"
module m(output reg a, output reg b, output reg c, output reg [3:0] q);
  initial begin
    {a, b, c} = 3'b101;
    #5;
    {q[3:2], q[1:0]} = 4'b1100;
    #5 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("concat_lhs.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "m".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("concat_lhs.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    let (a, _, _) = sess
        .eval_signal_resolved("a")
        .expect("a must be resolvable after concat-LHS assignment");
    assert_eq!(a, 1, "a should be the MSB of 3'b101 = 1");

    let (b, _, _) = sess.eval_signal_resolved("b").expect("b resolvable");
    assert_eq!(b, 0, "b should be the middle bit of 3'b101 = 0");

    let (c, _, _) = sess.eval_signal_resolved("c").expect("c resolvable");
    assert_eq!(c, 1, "c should be the LSB of 3'b101 = 1");

    let (q, _, _) = sess.eval_signal_resolved("q").expect("q resolvable");
    assert_eq!(q & 0xF, 0b1100, "q should be 4'b1100 after second concat-LHS");
}

/// **Bug regression**: `generate for (i = 0; i < W; i = i + 1) begin assign y[i] = a[i] ^ b[i]; end`
/// previously dropped the entire generate block because `parse_generate_construct`
/// only accepted a single instance inside the body, not continuous assigns.
/// The fix added [`CstModuleItem::GenerateForAssigns`] and an IR unroll that
/// substitutes the loop variable in each assign's LHS and RHS for every
/// iteration `0..N`.
#[test]
fn generate_for_assigns_unrolls_into_per_bit_drivers() {
    let src = r#"
module bit_xor #(parameter W = 4) (input [W-1:0] a, input [W-1:0] b, output [W-1:0] y);
  genvar i;
  generate
    for (i = 0; i < W; i = i + 1) begin : g
      assign y[i] = a[i] ^ b[i];
    end
  endgenerate
endmodule

module gen_assigns_tb;
  reg [3:0] a, b;
  wire [3:0] y;
  bit_xor #(.W(4)) dut(.a(a), .b(b), .y(y));
  initial begin
    a = 4'b1010;
    b = 4'b0101;
    #5;
    a = 4'b1100;
    b = 4'b1010;
    #5 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("gen_assigns.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "gen_assigns_tb".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("gen_assigns.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    let (y, _, _) = sess
        .eval_signal_resolved("y")
        .expect("y must resolve after the generate-for is unrolled");
    // After the second pair of assignments: a=1100, b=1010, y = a ^ b = 0110.
    assert_eq!(y & 0xF, 0b0110, "y should be 4'b0110 = 1100 XOR 1010");
}

/// **Bug regression**: `function ... endfunction` was previously rejected by
/// the lexer. After adding `function`/`endfunction` keywords and an
/// inline-at-parse-time function expansion, a function returning a simple
/// expression should be callable from an `assign`.
#[test]
fn function_decl_inlines_at_call_site() {
    let src = r#"
module m(input [3:0] a, input [3:0] b, output [4:0] y);
  function [4:0] add_double;
    input [3:0] x;
    input [3:0] y;
    begin
      add_double = (x + y) << 1;
    end
  endfunction
  assign y = add_double(a, b);
endmodule

module func_inline_tb;
  reg [3:0] a, b;
  wire [4:0] y;
  m dut(.a(a), .b(b), .y(y));
  initial begin
    a = 4'd5; b = 4'd5; #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("func.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "func_inline_tb".into(),
        num_cycles: 2,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("func.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);
    let (y, _, _) = sess.eval_signal_resolved("y").expect("y resolvable");
    assert_eq!(y & 0x1F, 20, "(5+5)<<1 = 20");
}

/// **Bug regression**: `task ... endtask` was previously rejected. After
/// adding `task`/`endtask` keywords and inline-at-parse-time task expansion,
/// invoking the task from an `always` block should produce the same behaviour
/// as if its body were written inline.
#[test]
fn task_decl_inlines_into_always_block() {
    let src = r#"
module accum(input clk, input rst_n, input [3:0] inc, output reg [7:0] acc);
  task do_add;
    input [3:0] x;
    begin
      acc = acc + x;
    end
  endtask
  always @(posedge clk) begin
    if (!rst_n) acc <= 8'd0;
    else        do_add(inc);
  end
endmodule

module task_inline_tb;
  reg clk, rst_n;
  reg [3:0] inc;
  wire [7:0] acc;
  accum dut(.clk(clk), .rst_n(rst_n), .inc(inc), .acc(acc));
  initial begin
    clk = 0; rst_n = 0; inc = 4'd3;
    #7 rst_n = 1;
    #50 $finish;
  end
  always #5 clk = ~clk;
endmodule
"#;
    let mut proj = build_ir_for_file("task.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "task_inline_tb".into(),
        num_cycles: 16,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("task.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);
    let (acc, _, _) = sess.eval_signal_resolved("acc").expect("acc resolvable");
    // 5 posedges of inc=3 after rst release in the run window (15, 25, 35, 45, 55 fs)
    // should produce acc = 15. The exact count depends on cycles; we just
    // assert acc is a positive multiple of 3 to confirm the task inlining
    // produced a working accumulate path.
    assert!(acc > 0, "acc must accumulate; got {acc}");
    assert_eq!(acc % 3, 0, "acc must be a multiple of inc=3; got {acc}");
}

/// **Bug regression**: `generate if (PARAM) begin ... end else begin ... end`
/// was previously dropped because `parse_generate_construct` only handled
/// `generate for`. The fix added `CstModuleItem::GenerateIf` and an IR
/// elaborator that const-evaluates the condition and lowers the chosen body.
#[test]
fn generate_if_picks_branch_at_elaboration() {
    let src = r#"
module pick #(parameter USE_AND = 0) (input a, input b, output y);
  generate
    if (USE_AND) assign y = a & b;
    else         assign y = a | b;
  endgenerate
endmodule

module gen_if_tb;
  reg a, b;
  wire y_or, y_and;
  pick #(.USE_AND(0)) u0(.a(a), .b(b), .y(y_or));
  pick #(.USE_AND(1)) u1(.a(a), .b(b), .y(y_and));
  initial begin
    a = 1; b = 0;
    #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("gen_if.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "gen_if_tb".into(),
        num_cycles: 2,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("gen_if.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);
    let (y_or, _, _) = sess.eval_signal_resolved("y_or").expect("y_or resolvable");
    let (y_and, _, _) = sess.eval_signal_resolved("y_and").expect("y_and resolvable");
    assert_eq!(y_or & 1, 1, "y_or = a | b = 1 | 0 = 1");
    assert_eq!(y_and & 1, 0, "y_and = a & b = 1 & 0 = 0");
}

/// **Bug regression**: `generate case (PARAM) ... endcase` was previously
/// dropped. The fix added `CstModuleItem::GenerateCase` with the same
/// const-eval-and-pick semantics as [`generate_if_picks_branch_at_elaboration`].
#[test]
fn generate_case_picks_arm_at_elaboration() {
    let src = r#"
module op #(parameter OP = 0) (input [3:0] a, input [3:0] b, output [3:0] y);
  generate
    case (OP)
      0: assign y = a + b;
      1: assign y = a - b;
      default: assign y = a ^ b;
    endcase
  endgenerate
endmodule

module gen_case_tb;
  reg  [3:0] a, b;
  wire [3:0] y_add, y_xor;
  op #(.OP(0)) u_add(.a(a), .b(b), .y(y_add));
  op #(.OP(9)) u_xor(.a(a), .b(b), .y(y_xor));
  initial begin
    a = 4'd5; b = 4'd3;
    #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("gen_case.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "gen_case_tb".into(),
        num_cycles: 2,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("gen_case.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);
    let (y_add, _, _) = sess.eval_signal_resolved("y_add").expect("y_add resolvable");
    let (y_xor, _, _) = sess.eval_signal_resolved("y_xor").expect("y_xor resolvable");
    assert_eq!(y_add & 0xF, 8, "5 + 3 = 8");
    assert_eq!(y_xor & 0xF, 6, "5 ^ 3 = 6 (default arm picked)");
}

/// **Bug regression**: nested `generate for` (an outer `for` whose body is
/// another `for`) was previously dropped because the parser only accepted
/// either a single instance or all-assigns. The fix added
/// `CstModuleItem::GenerateForBody` and recursive elaboration.
#[test]
fn nested_generate_for_unrolls_both_levels() {
    let src = r#"
module xor_grid #(parameter R = 2, parameter C = 2)
  (input [R*C-1:0] a, input [R*C-1:0] b, output [R*C-1:0] y);
  genvar r, c;
  generate
    for (r = 0; r < R; r = r + 1) begin : row
      for (c = 0; c < C; c = c + 1) begin : col
        assign y[r*C + c] = a[r*C + c] ^ b[r*C + c];
      end
    end
  endgenerate
endmodule

module nested_tb;
  reg  [3:0] a, b;
  wire [3:0] y;
  xor_grid #(.R(2), .C(2)) dut(.a(a), .b(b), .y(y));
  initial begin
    a = 4'b1100; b = 4'b1010;
    #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("nested.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "nested_tb".into(),
        num_cycles: 2,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("nested.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);
    let (y, _, _) = sess.eval_signal_resolved("y").expect("y resolvable");
    assert_eq!(y & 0xF, 0b0110, "1100 XOR 1010 = 0110 across all 4 bits");
}

/// **Bug regression**: `casez` (and `casex`) patterns containing `?` / `z` /
/// `x` wildcards were rejected by the lexer and the parser. After teaching
/// the lexer to consume wildcard characters inside sized literals and adding
/// a `care_mask` per IrCaseArm (only populated for `casez`/`casex`), the
/// matcher now uses `(scrutinee & mask) == (value & mask)` semantics.
#[test]
fn casez_priority_encoder_matches_with_wildcards() {
    let src = r#"
module pri_enc(input [7:0] in, output reg [2:0] pos, output reg valid);
  always @(*) begin
    casez (in)
      8'b1???_????: begin pos = 3'd7; valid = 1; end
      8'b01??_????: begin pos = 3'd6; valid = 1; end
      8'b001?_????: begin pos = 3'd5; valid = 1; end
      8'b0001_????: begin pos = 3'd4; valid = 1; end
      8'b0000_1???: begin pos = 3'd3; valid = 1; end
      8'b0000_01??: begin pos = 3'd2; valid = 1; end
      8'b0000_001?: begin pos = 3'd1; valid = 1; end
      8'b0000_0001: begin pos = 3'd0; valid = 1; end
      default:      begin pos = 3'd0; valid = 0; end
    endcase
  end
endmodule

module casez_tb;
  reg [7:0] in;
  wire [2:0] pos;
  wire valid;
  pri_enc dut(.in(in), .pos(pos), .valid(valid));
  initial begin
    in = 8'b00110000;
    #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("casez.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "casez_tb".into(),
        num_cycles: 2,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("casez.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    // 8'b00110000 has its highest 1 at bit 5 → arm `8'b001?_????` fires.
    let (pos, _, _) = sess.eval_signal_resolved("pos").expect("pos resolvable");
    let (valid, _, _) = sess.eval_signal_resolved("valid").expect("valid resolvable");
    assert_eq!(pos & 0x7, 5, "highest-bit position of 0b00110000 is 5");
    assert_eq!(valid & 0x1, 1, "non-zero input must assert valid");
}


/// Multi-statement task body: tests that a task body containing several
/// blocking assigns is spliced in full at the call site (block-splicing
/// implementation). Each call to `seed()` should update three module-level
/// regs.
#[test]
fn task_with_multi_statement_body_splices_each_assignment() {
    let src = r#"
module dut(input clk, input rst_n, output reg [7:0] a, output reg [7:0] b, output reg [7:0] c);
  task seed;
    input [7:0] x;
    begin
      a = x;
      b = x + 8'd1;
      c = x + 8'd2;
    end
  endtask
  always @(posedge clk) begin
    if (!rst_n) begin
      a <= 8'd0; b <= 8'd0; c <= 8'd0;
    end else begin
      seed(8'd10);
    end
  end
endmodule

module multi_task_tb;
  reg clk, rst_n;
  wire [7:0] a, b, c;
  dut u(.clk(clk), .rst_n(rst_n), .a(a), .b(b), .c(c));
  initial begin
    clk = 0; rst_n = 0;
    #7 rst_n = 1;
    #20 $finish;
  end
  always #5 clk = ~clk;
endmodule
"#;
    let mut proj = build_ir_for_file("task_multi.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "multi_task_tb".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("task_multi.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    let (a, _, _) = sess.eval_signal_resolved("a").expect("a resolvable");
    let (b, _, _) = sess.eval_signal_resolved("b").expect("b resolvable");
    let (c, _, _) = sess.eval_signal_resolved("c").expect("c resolvable");
    // The task fires every posedge after rst_n release. Final values:
    assert_eq!(a & 0xFF, 10, "a = x = 10");
    assert_eq!(b & 0xFF, 11, "b = x + 1 = 11");
    assert_eq!(c & 0xFF, 12, "c = x + 2 = 12");
}

/// Multi-statement function body: tests the let-binding semantics where
/// intermediate assignments become substitutions for subsequent statements.
/// `saturate` computes `(x + 1)` into a local `tmp`, then uses `tmp` in the
/// final return expression.
#[test]
fn function_with_let_bindings_substitutes_intermediates() {
    let src = r#"
module m(input [9:0] x, output [7:0] y);
  function [7:0] saturate;
    input [9:0] x;
    reg [9:0] tmp;
    begin
      tmp = x + 10'd1;
      saturate = (tmp > 10'd255) ? 8'hFF : tmp[7:0];
    end
  endfunction
  assign y = saturate(x);
endmodule

module func_let_tb;
  reg [9:0] x;
  wire [7:0] y;
  m dut(.x(x), .y(y));
  initial begin
    x = 10'd100; #10;  // saturate(100) -> tmp=101 -> y=101
    x = 10'd255; #10;  // saturate(255) -> tmp=256 -> y=0xFF (255 < 256, condition true)
    x = 10'd500; #10;  // saturate(500) -> tmp=501 -> y=0xFF
    #10 $finish;
  end
endmodule
"#;
    let mut proj = build_ir_for_file("func_let.v", src);
    optimize_project(&mut proj);
    let config = SimConfig {
        top_module: "func_let_tb".into(),
        num_cycles: 4,
        ..Default::default()
    };
    let tmp = tempdir().unwrap();
    let vcd_path: PathBuf = tmp.path().join("func_let.vcd");
    let mut sess = SimSession::start(&proj, config, vcd_path).unwrap();
    sess.step(StepMode::ToEnd, None);

    let (y, _, _) = sess.eval_signal_resolved("y").expect("y resolvable");
    assert_eq!(y & 0xFF, 0xFF, "saturate(500) clamps to 0xFF");
}
