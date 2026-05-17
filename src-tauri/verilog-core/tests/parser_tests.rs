use verilog_core::parse_file;
use verilog_core::{build_ir_for_file, IrBinOp, IrExpr, IrSensitivity, IrStmt};

#[test]
fn parses_module_name_and_ports_with_directions_and_ranges() {
    let src = r#"
module foo #(parameter WIDTH = 16) (
    input [WIDTH-1:0] data_in,
    output logic ready,
    inout [3:0] bus
);
endmodule
"#;

    let res = parse_file("foo.v", src);
    assert!(
        res.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        res.diagnostics
    );

    assert_eq!(res.modules.len(), 1);
    let m = &res.modules[0];
    assert_eq!(m.name, "foo");
    assert_eq!(m.ports.len(), 3);

    assert_eq!(m.ports[0].direction.as_deref(), Some("input"));
    assert_eq!(m.ports[0].name, "data_in");
    assert_eq!(m.ports[0].width, 16, "parametric [WIDTH-1:0] must evaluate to 16 bits");

    assert_eq!(m.ports[1].direction.as_deref(), Some("output"));
    assert_eq!(m.ports[1].name, "ready");

    assert_eq!(m.ports[2].direction.as_deref(), Some("inout"));
    assert_eq!(m.ports[2].name, "bus");
}

#[test]
fn ansi_port_list_repeats_direction_for_comma_separated_names() {
    let src = r#"
module m(
    output [6:0] TimerMSD, TimerLSD,
    output reg [6:0] ETL, NLTL, ELTL, WTL
);
endmodule
"#;
    let res = parse_file("m.v", src);
    assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    let m = &res.modules[0];
    assert_eq!(m.ports.len(), 6);
    for p in &m.ports {
        assert_eq!(
            p.direction.as_deref(),
            Some("output"),
            "port {} missing inherited output direction",
            p.name
        );
    }
    assert_eq!(m.ports[0].name, "TimerMSD");
    assert_eq!(m.ports[1].name, "TimerLSD");
    assert_eq!(m.ports[2].name, "ETL");
    assert_eq!(m.ports[5].name, "WTL");
    for p in &m.ports {
        assert_eq!(
            p.width, 7,
            "comma-separated ports must inherit [6:0] width for {}",
            p.name
        );
    }
}

#[test]
fn triple_gt_lowers_to_ashr_not_compare() {
    let src = r#"
module t;
  reg [22:0] PM;
  wire sh;
  always @(*) PM = sh ? PM : (PM >>> 1);
endmodule
"#;
    let proj = build_ir_for_file("t.v", src);
    let m = proj.modules.iter().find(|x| x.name == "t").expect("module t");
    let always = &m.always_blocks[0];
    let stmt = &always.stmts[0];
    let rhs = match stmt {
        verilog_core::IrStmt::BlockingAssign { rhs, .. } => rhs,
        _ => panic!("expected blocking assign"),
    };
    let tern = match rhs {
        IrExpr::Ternary { else_expr, .. } => else_expr,
        _ => panic!("expected ternary rhs, got {:?}", rhs),
    };
    assert!(
        matches!(
            tern.as_ref(),
            IrExpr::Binary {
                op: IrBinOp::Ashr,
                ..
            }
        ),
        "expected >>> in else arm to be IrBinOp::Ashr, got {:?}",
        tern
    );
}

#[test]
fn fourfunccalc_combinational_case_has_all_state_arms() {
    let path = "/Users/mihirepel/eecs270/Project 7/FourFuncCalc.v";
    if !std::path::Path::new(path).exists() {
        return;
    }
    let src = std::fs::read_to_string(path).unwrap();
    let proj = build_ir_for_file(path, &src);
    let m = proj
        .modules
        .iter()
        .find(|x| x.name == "FourFuncCalc")
        .expect("FourFuncCalc");
    let star: Vec<_> = m
        .always_blocks
        .iter()
        .filter(|ab| matches!(ab.sensitivity, IrSensitivity::Star))
        .collect();
    assert_eq!(star.len(), 1, "expected one always @* (next-state case)");
    let arms = star[0]
        .stmts
        .iter()
        .find_map(|s| {
            if let IrStmt::Case { arms, .. } = s {
                Some(arms.as_slice())
            } else {
                None
            }
        })
        .expect("case stmt");
    assert_eq!(
        arms.len(),
        21,
        "FourFuncCalc case(X) should have 21 arms (XInit..XDisp); parser/IR mismatch if not"
    );

    fn count_x_next_assigns(stmts: &[IrStmt]) -> usize {
        let mut n = 0;
        for s in stmts {
            match s {
                IrStmt::IfElse {
                    then_body,
                    else_body,
                    ..
                } => {
                    n += count_x_next_assigns(then_body);
                    n += count_x_next_assigns(else_body);
                }
                IrStmt::Case {
                    arms,
                    default,
                    ..
                } => {
                    for a in arms {
                        n += count_x_next_assigns(&a.body);
                    }
                    n += count_x_next_assigns(default);
                }
                IrStmt::NonBlockingAssign { lhs, .. } | IrStmt::BlockingAssign { lhs, .. } => {
                    if lhs == "X_Next" {
                        n += 1;
                    }
                }
                _ => {}
            }
        }
        n
    }

    // XAdd = 4: five distinct next-state assignments in the if / else-if ladder (see source).
    let xadd = arms
        .iter()
        .find(|a| a.value == IrExpr::Const(4))
        .expect("XAdd arm");
    assert_eq!(
        count_x_next_assigns(&xadd.body),
        5,
        "XAdd arm should contain 5 X_Next assignments (Equals, -, *, /, hold)"
    );
}

#[test]
fn instance_hash_parameter_elaborates_specialized_module() {
    let src = r#"
module AddSub #(parameter W = 16) ();
endmodule
module Parent #(parameter W = 11) ();
  AddSub #(.W(W)) u1 ();
endmodule
"#;
    let proj = build_ir_for_file("t.v", src);
    assert!(
        proj.modules.iter().any(|m| m.name == "AddSub__p_W_11"),
        "expected AddSub specialized to W=11, modules: {:?}",
        proj.modules.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    let parent = proj
        .modules
        .iter()
        .find(|m| m.name == "Parent")
        .expect("Parent");
    let inst = parent
        .instances
        .iter()
        .find(|i| i.instance_name == "u1")
        .expect("u1");
    assert_eq!(
        inst.module_name, "AddSub__p_W_11",
        "instance should target elaborated child"
    );
    assert!(
        inst.parameter_assignments.is_empty(),
        "params should be cleared after elaboration"
    );
}

#[test]
fn signed_call_parses_to_ir() {
    let src = r#"
module t;
  wire [11:0] x;
  wire [11:0] y;
  assign y = $signed(x);
endmodule
"#;
    let proj = build_ir_for_file("t.v", src);
    let m = proj.modules.iter().find(|x| x.name == "t").expect("module t");
    let a = m.assigns.iter().find(|a| a.lhs == "y").expect("assign y");
    assert!(
        matches!(a.rhs, IrExpr::Signed(_)),
        "expected $signed to become IrExpr::Signed, got {:?}",
        a.rhs
    );
}


#[test]
fn statement_spans_cover_assignment_byte_range() {
    use verilog_core::source_map::SourceMap;
    let src = "module t(input a, output reg y);\n  always @(*) y = a;\nendmodule\n";
    let mut sm = SourceMap::new();
    let file_id = sm.intern("t.v", src);
    let proj = build_ir_for_file("t.v", src);
    let m = proj.modules.iter().find(|x| x.name == "t").expect("module t");
    let ab = m.always_blocks.first().expect("at least one always");
    let (_stmt, span) = ab.stmts.iter_with_spans().next().expect("at least one stmt");
    assert_eq!(span.file_id, proj.modules[0].file_id);
    let _ = file_id; // SourceMap precompute check (intern doesn't fail)
    let text = &src[span.start as usize..span.end as usize];
    assert!(
        text.contains("y = a"),
        "span should cover the assignment, got: {:?}",
        text
    );
}
