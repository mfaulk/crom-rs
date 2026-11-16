use calc::{calculate, CalcError, EvalError};

#[test]
fn simple_arithmetic() {
    assert_eq!(calculate("1 + 2 * 3"), Ok(7));
    assert_eq!(calculate("(1 + 2) * 3"), Ok(9));
}

#[test]
fn multi_digit_operands() {
    assert_eq!(calculate("10 + 20"), Ok(30));
    assert_eq!(calculate("100 / 7"), Ok(14));
}

#[test]
fn chained_subtraction() {
    assert_eq!(calculate("10 - 3 - 2"), Ok(5));
}

#[test]
fn unary_minus() {
    assert_eq!(calculate("-3 + 5"), Ok(2));
    assert_eq!(calculate("-(2 * 4)"), Ok(-8));
}

#[test]
fn division_by_zero_is_an_error() {
    assert_eq!(
        calculate("1 / (2 - 2)"),
        Err(CalcError::Eval(EvalError::DivisionByZero))
    );
}
