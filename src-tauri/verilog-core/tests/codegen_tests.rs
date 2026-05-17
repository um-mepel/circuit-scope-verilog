use std::path::PathBuf;
use verilog_core::{
    build_ir_for_file, generate_vcd, optimize_project, IrProject, SimConfig,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("codegen_fixtures")
        .join(name)
}

fn build_and_gen(filename: &str, top: &str, cycles: usize) -> String {
    let path = fixture(filename);
    let src = std::fs::read_to_string(&path).unwrap();
    let mut ir = build_ir_for_file(path.to_string_lossy(), &src);
    optimize_project(&mut ir);
    let config = SimConfig {
        top_module: top.into(),
        num_cycles: cycles,
        timescale: "1ns".into(),
        clock_half_period: 5,
        ..Default::default()
    };
    generate_vcd(&ir, &config).expect("VCD generation should succeed")
}

/// VCD identifier token for a dumped variable named `name` (e.g. `HEX6`).
fn vcd_var_id_for_name(vcd: &str, name: &str) -> Option<String> {
    for line in vcd.lines() {
        let s = line.trim();
        if !s.starts_with("$var ") {
            continue;
        }
        let parts: Vec<&str> = s.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|p| *p == name) {
            if idx > 0 {
                return Some(parts[idx - 1].to_string());
            }
        }
    }
    None
}

/// Every `b… <id>` vector dump line for this VCD identifier, in file order.
fn vcd_bus_bitstrings_for_id(vcd: &str, id: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in vcd.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix('b') else {
            continue;
        };
        let Some((bits, sid)) = rest.split_once(' ') else {
            continue;
        };
        if bits.chars().all(|c| c == '0' || c == '1') && sid == id {
            out.push(bits.to_string());
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
// VCD format structure
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn vcd_has_required_sections() {
    let vcd = build_and_gen("comb_logic.v", "comb_logic", 2);
    assert!(vcd.contains("$date"), "missing $date");
    assert!(vcd.contains("$version"), "missing $version");
    assert!(
        vcd.contains("$timescale")
            && (vcd.contains("1ns") || vcd.contains("1 ns"))
            && vcd.contains("$end"),
        "missing $timescale block"
    );
    assert!(vcd.contains("$scope module comb_logic $end"), "missing $scope");
    assert!(vcd.contains("$upscope $end"), "missing $upscope");
    assert!(vcd.contains("$enddefinitions $end"), "missing $enddefs");
    assert!(vcd.contains("$dumpvars"), "missing $dumpvars");
    assert!(vcd.contains("#0"), "missing initial timestamp");
    assert!(vcd.contains("$comment"), "debug $comment headers should be present");
    assert!(
        vcd.contains("top_source_file:"),
        "IR-derived header should list top source path"
    );
    assert!(
        !vcd.contains("command_line:"),
        "fixture runs omit command line until caller sets VcdRunMeta"
    );
}

/// VCD **`$timescale`** uses the precision operand; **`#`** timestamps are **precision** ticks (IEEE-style).
#[test]
fn vcd_timescale_header_uses_precision_cosmetic() {
    let path = fixture("counter.v");
    let src = std::fs::read_to_string(&path).unwrap();
    let mut ir = build_ir_for_file(path.to_string_lossy(), &src);
    optimize_project(&mut ir);
    let config = SimConfig {
        top_module: "counter".into(),
        num_cycles: 3,
        timescale: "1ns".into(),
        timescale_precision: "1ps".into(),
        clock_half_period: 5,
        ..Default::default()
    };
    let vcd = generate_vcd(&ir, &config).expect("VCD generation should succeed");
    assert!(
        vcd.contains("1 ps"),
        "expected normalized precision on $timescale line"
    );
    assert!(
        vcd.contains("#5000") && vcd.contains("#10000"),
        "1ns unit with 1ps precision: half-period 5ns → 5000ps grid steps"
    );
}

#[test]
fn vcd_declares_all_signals() {
    let vcd = build_and_gen("comb_logic.v", "comb_logic", 2);
    assert!(vcd.contains("$var wire"), "should declare wire variables");
    for sig in &["a", "b", "y", "z"] {
        assert!(vcd.contains(sig), "missing signal {}", sig);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Combinational logic
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn comb_logic_generates_valid_vcd() {
    let vcd = build_and_gen("comb_logic.v", "comb_logic", 3);
    // Pure combinational — no clock, so values settle at time 0.
    assert!(vcd.contains("#0"));
    // Should have valid VCD content (not empty after header).
    assert!(vcd.len() > 100);
}

// ═══════════════════════════════════════════════════════════════════════
// Sequential counter
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn counter_produces_clock_edges() {
    let vcd = build_and_gen("counter.v", "counter", 5);
    // Clock from `always #5 clk = ~clk` in the fixture (no synthetic driver).
    assert!(vcd.contains("clk"), "clock signal should be in VCD");
    // Should have timestamps at half-period intervals.
    assert!(vcd.contains("#5"));
    assert!(vcd.contains("#10"));
    assert!(vcd.contains("#15"));
}

#[test]
fn counter_declares_reg_for_sequential() {
    let vcd = build_and_gen("counter.v", "counter", 3);
    assert!(vcd.contains("$var reg"), "count should be declared as reg");
}

#[test]
fn counter_has_multiple_value_changes() {
    let vcd = build_and_gen("counter.v", "counter", 10);
    let timestamp_count = vcd.matches("\n#").count();
    assert!(
        timestamp_count >= 5,
        "should have many timestamps, got {}",
        timestamp_count
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Hierarchy with instance
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn hierarchy_produces_vcd() {
    let vcd = build_and_gen("hierarchy.v", "top_hier", 3);
    assert!(vcd.contains("$scope module top_hier $end"));
    // After inlining, z should be assigned.
    assert!(vcd.contains("z"));
    assert!(vcd.contains("x"));
}

/// Full-range slice on instance output (`.hex(HEX6[6:0])`) must drive the port, not only `.hex(HEX7)`.
#[test]
fn instance_output_full_slice_drives_port() {
    let vcd = build_and_gen("instance_output_partselect.v", "instance_output_partselect", 2);
    assert!(
        vcd.contains("HEX7") && vcd.contains("HEX6"),
        "both outputs should appear in VCD"
    );
    let n_seven_f = vcd.matches("b1111111").count();
    assert!(
        n_seven_f >= 2,
        "both HEX7 and HEX6 should dump 7'h7F (b1111111); got {} matches:\n{}",
        n_seven_f,
        &vcd[..vcd.len().min(2000)]
    );
}

/// Part-select on instance output must track the same waveform as a plain connected port
/// (catches undriven slice where HEX6 would stick at X/0 while HEX7 toggles).
#[test]
fn instance_output_full_slice_matches_plain_port_when_dynamic() {
    let vcd = build_and_gen(
        "instance_output_partselect_dynamic.v",
        "instance_output_partselect_dynamic",
        48,
    );
    let id7 = vcd_var_id_for_name(&vcd, "HEX7").expect("HEX7 in VCD header");
    let id6 = vcd_var_id_for_name(&vcd, "HEX6").expect("HEX6 in VCD header");
    let seq7 = vcd_bus_bitstrings_for_id(&vcd, &id7);
    let seq6 = vcd_bus_bitstrings_for_id(&vcd, &id6);
    assert!(
        seq7.len() >= 4,
        "expected several HEX7 bus updates, got {}:\n{}",
        seq7.len(),
        &vcd[vcd.len().saturating_sub(3500)..]
    );
    assert_eq!(
        seq7, seq6,
        "HEX6 via .hex(HEX6[6:0]) must match HEX7 bit-for-bit over time (seq7={seq7:?} seq6={seq6:?})"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Error handling
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn missing_top_module_is_error() {
    let path = fixture("comb_logic.v");
    let src = std::fs::read_to_string(&path).unwrap();
    let ir = build_ir_for_file(path.to_string_lossy(), &src);
    let config = SimConfig {
        top_module: "nonexistent".into(),
        ..SimConfig::default()
    };
    let result = generate_vcd(&ir, &config);
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════
// Multi-bit signals
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn multibit_declares_width_and_range() {
    let vcd = build_and_gen("multibit.v", "multibit", 3);
    assert!(vcd.contains("[7:0]"), "should declare 8-bit range for data_in/data_out");
    assert!(vcd.contains("[3:0]"), "should declare 4-bit range for nibble");
}

#[test]
fn multibit_uses_binary_format() {
    let vcd = build_and_gen("multibit.v", "multibit", 3);
    assert!(vcd.contains("b"), "should use binary format for multi-bit signals");
}

// ═══════════════════════════════════════════════════════════════════════
// Initial blocks with #delay
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn initial_block_produces_timed_events() {
    let vcd = build_and_gen("initial_tb.v", "initial_tb", 10);
    assert!(vcd.contains("$dumpvars"), "should have $dumpvars");
    assert!(vcd.contains("#0"), "should have time 0");
    assert!(vcd.contains("#10"), "should have time 10 for first delay");
}

#[test]
fn initial_block_signals_are_reg() {
    let vcd = build_and_gen("initial_tb.v", "initial_tb", 5);
    assert!(vcd.contains("$var reg"), "initial block signals should be reg");
}

// ═══════════════════════════════════════════════════════════════════════
// x-initialization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn undriven_signals_show_x() {
    let vcd = build_and_gen("comb_logic.v", "comb_logic", 1);
    assert!(vcd.contains("x"), "undriven inputs should initially be x");
}

// ═══════════════════════════════════════════════════════════════════════
// Nested hierarchical scopes
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn nested_hierarchy_has_scopes() {
    let vcd = build_and_gen("nested_hier.v", "top_nested", 3);
    assert!(vcd.contains("$scope module top_nested $end"), "missing top scope");
    let upscope_count = vcd.matches("$upscope $end").count();
    assert!(upscope_count >= 2, "should have nested $upscope entries, got {}", upscope_count);
}

// ═══════════════════════════════════════════════════════════════════════
// VCD polish: $dumpvars after #0, real date
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn dumpvars_placement() {
    let vcd = build_and_gen("counter.v", "counter", 2);
    let t0 = vcd.find("#0").unwrap();
    let dv = vcd.find("$dumpvars").unwrap();
    assert!(dv > t0, "$dumpvars should come after #0");
}

#[test]
fn vcd_has_date_section() {
    let vcd = build_and_gen("counter.v", "counter", 1);
    assert!(vcd.contains("$date"));
    assert!(vcd.contains("$end"));
}

/// Verilog (1364) style is `wire`/`reg`; stray `logic` keywords (non-1364, common in mixed sources)
/// must still lex as net declarations or they are misparsed as instances and never simulate.
#[test]
fn logic_keyword_declares_internal_nets_in_ir() {
    let src = r#"
module m(input wire clk);
  logic [1:0] state;
  always @(posedge clk) state <= state + 1;
endmodule
"#;
    let ir = build_ir_for_file("logic_kw.v", src);
    let errs: Vec<_> = ir
        .diagnostics
        .iter()
        .filter(|d| matches!(d.severity, verilog_core::Severity::Error))
        .collect();
    assert!(errs.is_empty(), "parse errors: {:?}", errs);
    let m = ir
        .modules
        .iter()
        .find(|x| x.name == "m")
        .expect("module m");
    assert!(
        m.nets.iter().any(|n| n.name == "state" && n.width == 2),
        "expected net state width 2, have nets: {:?}",
        m.nets
    );
}

/// Packed bit-select and port expressions like `.p(sw[i])` must not collapse to the whole vector
/// (which would incorrectly use the LSBs after masking).
#[test]
fn bit_select_uses_correct_bit_index() {
    let src = r#"
module top;
  reg [3:0] sw;
  wire o;
  assign o = sw[3];
  initial sw = 4'b1010;
endmodule
"#;
    let mut ir = build_ir_for_file("bit_sel.v", src);
    optimize_project(&mut ir);
    let config = SimConfig {
        top_module: "top".into(),
        num_cycles: 2,
        timescale: "1ns".into(),
        clock_half_period: 5,
        ..Default::default()
    };
    let vcd = generate_vcd(&ir, &config).expect("vcd");
    let d0 = vcd.find("$dumpvars").unwrap();
    let d1 = vcd[d0..].find("$end").unwrap() + d0;
    let dump = &vcd[d0..d1];
    assert!(
        dump.contains('1'),
        "sw=1010 so sw[3]=1; dump should not be all zero: {}",
        dump
    );
}

/// Child **Clock** is a continuous net driven from parent **clk**. Sequential `posedge` must see
/// the net updated after `clk` toggles — otherwise only the generator clock appears in the VCD
/// (regressions: full-chip TB with `TLC` clock port).
#[test]
fn hierarchical_clock_propagates_before_posedge_fires() {
    let src = r#"
module child(input wire Clock, output reg [3:0] cnt);
  always @(posedge Clock) cnt <= cnt + 4'd1;
endmodule
module top;
  reg clk;
  wire [3:0] q;
  child u(.Clock(clk), .cnt(q));
  initial clk = 0;
  always #5 clk = ~clk;
endmodule
"#;
    let mut ir = build_ir_for_file("hier_clk.v", src);
    optimize_project(&mut ir);
    let config = SimConfig {
        top_module: "top".into(),
        num_cycles: 24,
        timescale: "1ns".into(),
        clock_half_period: 5,
        ..Default::default()
    };
    let vcd = generate_vcd(&ir, &config).expect("vcd");
    assert!(
        vcd.contains("b1010 ") || vcd.contains("b1010$"),
        "after many posedges cnt should reach ~10; VCD snippet:\n{}",
        vcd.chars().take(3500).collect::<String>()
    );
}

/// `localparam` must fold into the IR (the parser used to skip it), or `initial t = ten` schedules 0
/// and sequential FSMs never leave reset-like state (Project 6 / TLC style).
#[test]
fn procedural_bit_select_in_initial_updates_packed_reg() {
    let src = r#"
module top;
  reg [3:0] r;
  initial begin
    r = 4'b0;
    r[2] = 1'b1;
  end
endmodule
"#;
    let mut ir = build_ir_for_file("bitsel.v", src);
    optimize_project(&mut ir);
    let vcd = generate_vcd(
        &ir,
        &SimConfig {
            top_module: "top".into(),
            num_cycles: 1,
            timescale: "1ns".into(),
            clock_half_period: 5,
            ..Default::default()
        },
    )
    .expect("vcd");
    assert!(
        vcd.contains("b100 ")
            || vcd.contains("b100\n")
            || vcd.lines().any(|l| l.starts_with("b100")),
        "expected r=4 after r[2]=1; excerpt:\n{}",
        vcd.chars().take(900).collect::<String>()
    );
}

#[test]
fn localparam_initial_schedules_constant_rhs() {
    let src = r#"
module top;
  localparam ten = 4'd10;
  reg [3:0] t;
  reg clk;
  initial begin
    t = ten;
    clk = 0;
  end
  always #5 clk = ~clk;
  always @(posedge clk) t <= t - 4'd1;
endmodule
"#;
    let mut ir = build_ir_for_file("lp.v", src);
    optimize_project(&mut ir);
    let vcd = generate_vcd(
        &ir,
        &SimConfig {
            top_module: "top".into(),
            num_cycles: 4,
            timescale: "1ns".into(),
            clock_half_period: 5,
            ..Default::default()
        },
    )
    .expect("vcd");
    assert!(
        vcd.contains("b1010 ") || vcd.lines().any(|l| l.starts_with("b1010")),
        "dumpvars should show t=10, not 0; excerpt:\n{}",
        vcd.chars().take(1200).collect::<String>()
    );
}

/// Positional `mod i (a, b)` must bind ports; otherwise child inputs stay undriven (X).
#[test]
fn positional_port_instance_connects_wires() {
    let child_src = r#"
module leaf(input wire a, output wire y);
  assign y = a;
endmodule
"#;
    let top_src = r#"
module top;
  wire x, z;
  leaf u (x, z);
  assign x = 1;
endmodule
"#;
    let mut project = IrProject {
        modules: vec![],
        diagnostics: vec![],
        source_map: verilog_core::source_map::SourceMap::new(),
    };
    let mut c = build_ir_for_file("leaf.v", child_src);
    let mut t = build_ir_for_file("top.v", top_src);
    project.modules.append(&mut c.modules);
    project.modules.append(&mut t.modules);
    project.diagnostics.extend(c.diagnostics);
    project.diagnostics.extend(t.diagnostics);
    optimize_project(&mut project);
    let vcd = generate_vcd(
        &project,
        &SimConfig {
            top_module: "top".into(),
            num_cycles: 2,
            timescale: "1ns".into(),
            clock_half_period: 5,
            ..Default::default()
        },
    )
    .expect("vcd");
    let d0 = vcd.find("$dumpvars").unwrap();
    let chunk = &vcd[d0..d0 + 500.min(vcd.len() - d0)];
    assert!(
        !chunk.contains("xz ") && !chunk.contains("bx "),
        "positional ports should drive leaf inputs so z is not X; excerpt:\n{}",
        chunk
    );
}

/// Blocking assigns in `initial` to packed vectors must see prior bit updates when scheduling.
#[test]
fn initial_sequential_packed_bit_assigns_see_prior_blocking_updates() {
    let src = r#"
module tb;
  reg [3:0] r;
  initial begin
    r[0] = 1;
    r[1] = 1;
  end
endmodule
"#;
    let mut ir = build_ir_for_file("tb.v", src);
    optimize_project(&mut ir);
    let vcd = generate_vcd(
        &ir,
        &SimConfig {
            top_module: "tb".into(),
            num_cycles: 1,
            timescale: "1ns".into(),
            clock_half_period: 5,
            ..Default::default()
        },
    )
    .expect("vcd");
    let after = vcd.find("$dumpvars").unwrap();
    let chunk = &vcd[after..after + 500.min(vcd.len() - after)];
    assert!(
        chunk.contains("b11 ")
            || chunk.contains("b11\t")
            || chunk.contains("b0011 ")
            || chunk.contains("b0011\t"),
        "expected r == 3 after two blocking bit assigns, got:\n{}",
        chunk
    );
}

// ═══════════════════════════════════════════════════════════════════════
// All fixtures smoke test
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn all_codegen_fixtures_produce_valid_vcd() {
    let fixtures = [
        ("comb_logic.v", "comb_logic"),
        ("counter.v", "counter"),
        ("hierarchy.v", "top_hier"),
        ("multibit.v", "multibit"),
        ("initial_tb.v", "initial_tb"),
        ("nested_hier.v", "top_nested"),
    ];
    for (file, top) in &fixtures {
        let vcd = build_and_gen(file, top, 5);
        assert!(
            vcd.contains("$enddefinitions"),
            "{} missing $enddefinitions",
            file
        );
        assert!(vcd.contains("#0"), "{} missing #0", file);
    }
}
