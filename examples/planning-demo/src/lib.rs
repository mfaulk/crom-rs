//! Tiny math parser used by the M11 planning demo.
//!
//! The fix lives in `evaluate_op`: the planning agent should propose a
//! plan that inspects this file, edits the bug, and re-runs `cargo test`
//! as the validation step.

pub fn evaluate_op(left: i64, op: char, right: i64) -> Result<i64, &'static str> {
    match op {
        '+' => Ok(left + right),
        '-' => Ok(left - right),
        '*' => Ok(left * right),
        '/' => {
            if right == 0 {
                return Err("division by zero");
            }
            // Bug: integer division is correct here; the broken branch above
            // was previously this match arm returning the wrong sign. The
            // failing test below is `subtract_works` — flipping `-` to `+`
            // (planted bug) yields the wrong answer.
            Ok(left / right)
        }
        _ => Err("unknown operator"),
    }
}

/// Planted bug: subtract is implemented as addition. The demo's failing
/// test will exercise `subtract_works`; the planning agent should:
///
/// 1. Read `src/lib.rs` and `tests/`.
/// 2. Identify the bug in `subtract`.
/// 3. Plan a single Edit to `src/lib.rs`.
/// 4. Run `cargo test` as validation.
pub fn subtract(left: i64, right: i64) -> i64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_works() {
        assert_eq!(evaluate_op(2, '+', 3).unwrap(), 5);
    }

    #[test]
    fn multiply_works() {
        assert_eq!(evaluate_op(4, '*', 3).unwrap(), 12);
    }

    #[test]
    fn divide_works() {
        assert_eq!(evaluate_op(10, '/', 2).unwrap(), 5);
    }

    #[test]
    fn divide_by_zero_is_an_error() {
        assert!(evaluate_op(1, '/', 0).is_err());
    }

    #[test]
    fn unknown_op_is_an_error() {
        assert!(evaluate_op(1, '?', 2).is_err());
    }

    // Planted failure: tests M11 plan/approve/act path.
    #[test]
    fn subtract_works() {
        assert_eq!(subtract(10, 3), 7);
    }
}
