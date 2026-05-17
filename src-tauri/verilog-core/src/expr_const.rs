//! Constant folding for `parser::Expr` (parameters, localparams, generate bounds).
use std::collections::HashMap;

use crate::parser::{BinaryOp, Expr, UnaryOp};

/// IEEE 1364 `$clog2(n)`: minimum bits to represent `n` states; `n` is treated as integer ≥ 1.
pub(crate) fn verilog_clog2(n: i64) -> i64 {
    if n <= 1 {
        0
    } else {
        let u = (n - 1) as u64;
        64 - u.leading_zeros() as i64
    }
}

pub fn parse_verilog_number(s: &str) -> i64 {
    if let Some(pos) = s.find('\'') {
        // IEEE 1364: [size] '<base_format> digits
        // base_format is optional `s` or `S` (signed interpretation) then b|d|h|o.
        let mut after = &s[pos + 1..];
        if after.starts_with('s') || after.starts_with('S') {
            after = &after[1..];
        }
        let (radix, digits) = if after.starts_with('d') || after.starts_with('D') {
            (10, &after[1..])
        } else if after.starts_with('h') || after.starts_with('H') {
            (16, &after[1..])
        } else if after.starts_with('b') || after.starts_with('B') {
            (2, &after[1..])
        } else if after.starts_with('o') || after.starts_with('O') {
            (8, &after[1..])
        } else {
            (10, after)
        };
        // For `casez`/`casex` patterns we accept `?` / `x` / `z` as wildcards
        // in the literal. The numeric value treats those positions as 0; the
        // case-arm matcher reconstructs the care-mask separately.
        let clean: String = digits
            .chars()
            .filter(|c| *c != '_')
            .map(|c| match c {
                '?' | 'x' | 'X' | 'z' | 'Z' => '0',
                other => other,
            })
            .collect();
        i64::from_str_radix(&clean, radix).unwrap_or(0)
    } else {
        let clean: String = s.chars().filter(|c| *c != '_').collect();
        clean.parse::<i64>().unwrap_or(0)
    }
}

fn eval_binop_for_localparam(op: BinaryOp, l: i64, r: i64) -> i64 {
    match op {
        BinaryOp::Add => l.wrapping_add(r),
        BinaryOp::Sub => l.wrapping_sub(r),
        BinaryOp::Mul => l.wrapping_mul(r),
        BinaryOp::Div => {
            if r == 0 {
                0
            } else {
                l.wrapping_div(r)
            }
        }
        BinaryOp::Mod => {
            if r == 0 {
                0
            } else {
                l.wrapping_rem(r)
            }
        }
        BinaryOp::And => l & r,
        BinaryOp::Or => l | r,
        BinaryOp::Xor => l ^ r,
        BinaryOp::Shl => l.wrapping_shl((r as u32).min(63)),
        BinaryOp::Shr => l.wrapping_shr((r as u32).min(63)),
        BinaryOp::Ashr => crate::arith::arith_shr_i64(l, (r as u32).min(63), 64),
        BinaryOp::LogAnd => i64::from(l != 0 && r != 0),
        BinaryOp::LogOr => i64::from(l != 0 || r != 0),
        BinaryOp::Eq => i64::from(l == r),
        BinaryOp::Ne => i64::from(l != r),
        BinaryOp::Lt => i64::from(l < r),
        BinaryOp::Le => i64::from(l <= r),
        BinaryOp::Gt => i64::from(l > r),
        BinaryOp::Ge => i64::from(l >= r),
    }
}

pub(crate) fn const_eval_param_expr(e: &Expr, known: &HashMap<String, i64>) -> Option<i64> {
    match e {
        Expr::Number(s) => Some(parse_verilog_number(s)),
        Expr::Ident(name) => known.get(name).copied(),
        Expr::Unary { op, operand } => {
            let v = const_eval_param_expr(operand, known)?;
            match op {
                UnaryOp::Pos => Some(v),
                UnaryOp::Neg => Some(v.wrapping_neg()),
                UnaryOp::Not => Some(!v),
                UnaryOp::LogNot => Some(i64::from(v == 0)),
            }
        }
        Expr::Binary { op, left, right } => {
            let l = const_eval_param_expr(left, known)?;
            let r = const_eval_param_expr(right, known)?;
            Some(eval_binop_for_localparam(*op, l, r))
        }
        Expr::Ternary {
            cond,
            then_expr,
            else_expr,
        } => {
            let c = const_eval_param_expr(cond, known)?;
            if c != 0 {
                const_eval_param_expr(then_expr, known)
            } else {
                const_eval_param_expr(else_expr, known)
            }
        }
        Expr::Clog2(arg) => {
            let v = const_eval_param_expr(arg, known)?;
            Some(verilog_clog2(v))
        }
        Expr::Signed(inner) => {
            let v = const_eval_param_expr(inner, known)?;
            // Width of inner is not tracked here; 32-bit reinterpretation matches typical unsized folds.
            Some(crate::arith::sign_extend_i64(v, 32))
        }
        Expr::Concat(_) | Expr::Index { .. } => None,
    }
}

pub(crate) fn resolve_local_param_values(pairs: &[(String, Expr)]) -> HashMap<String, i64> {
    let mut raw: HashMap<String, Expr> = HashMap::new();
    for (n, e) in pairs {
        raw.insert(n.clone(), e.clone());
    }
    let mut resolved: HashMap<String, i64> = HashMap::new();
    let cap = raw.len().max(1).saturating_mul(8);
    for _ in 0..cap {
        let mut progress = false;
        let keys: Vec<String> = raw.keys().cloned().collect();
        for k in keys {
            if resolved.contains_key(&k) {
                continue;
            }
            let Some(e) = raw.get(&k) else {
                continue;
            };
            if let Some(v) = const_eval_param_expr(e, &resolved) {
                resolved.insert(k, v);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    resolved
}

#[cfg(test)]
mod parse_number_tests {
    use std::collections::HashMap;

    use crate::parser::{Expr, UnaryOp};

    use super::{const_eval_param_expr, parse_verilog_number};

    #[test]
    fn sized_signed_decimal_parses_magnitude() {
        assert_eq!(parse_verilog_number("11'sd3"), 3);
        assert_eq!(parse_verilog_number("11'SD3"), 3);
    }

    #[test]
    fn sized_signed_hex_after_s_marker() {
        assert_eq!(parse_verilog_number("8'shFF"), 255);
    }

    #[test]
    fn unsized_unsigned_still_works() {
        assert_eq!(parse_verilog_number("11'd3"), 3);
    }

    #[test]
    fn unary_neg_of_signed_sized_decimal_is_negative() {
        let e = Expr::Unary {
            op: UnaryOp::Neg,
            operand: Box::new(Expr::Number("11'sd3".into())),
        };
        assert_eq!(const_eval_param_expr(&e, &HashMap::new()), Some(-3));
    }
}
