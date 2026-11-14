use crate::parser::Expr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    DivisionByZero,
    Overflow,
}

/// Evaluate with checked integer arithmetic. Division truncates toward zero.
pub fn evaluate(expr: &Expr) -> Result<i64, EvalError> {
    let bin =
        |a: &Expr, b: &Expr| -> Result<(i64, i64), EvalError> { Ok((evaluate(a)?, evaluate(b)?)) };
    match expr {
        Expr::Num(n) => Ok(*n),
        Expr::Neg(e) => evaluate(e)?.checked_neg().ok_or(EvalError::Overflow),
        Expr::Add(a, b) => {
            let (x, y) = bin(a, b)?;
            x.checked_add(y).ok_or(EvalError::Overflow)
        }
        Expr::Sub(a, b) => {
            let (x, y) = bin(a, b)?;
            x.checked_sub(y).ok_or(EvalError::Overflow)
        }
        Expr::Mul(a, b) => {
            let (x, y) = bin(a, b)?;
            x.checked_mul(y).ok_or(EvalError::Overflow)
        }
        Expr::Div(a, b) => {
            let (x, y) = bin(a, b)?;
            if y == 0 {
                return Err(EvalError::DivisionByZero);
            }
            x.checked_div(y).ok_or(EvalError::Overflow)
        }
    }
}
