use calc::{calculate, parse, tokenize, CalcError, EvalError, Expr, ParseError, Token};

#[test]
fn left_associativity_everywhere() {
    assert_eq!(calculate("100 - 20 - 30 - 5"), Ok(45));
    assert_eq!(calculate("100 / 10 / 5"), Ok(2));
    assert_eq!(calculate("64 / 4 * 2"), Ok(32));
    assert_eq!(calculate("2 * 30 / 4"), Ok(15));
    assert_eq!(calculate("1 - 2 + 3"), Ok(2));
}

#[test]
fn long_literals_and_nesting() {
    assert_eq!(calculate("2 * (3 + 40)"), Ok(86));
    assert_eq!(calculate("1234567 + 1"), Ok(1234568));
    assert_eq!(calculate("((7))"), Ok(7));
    assert_eq!(calculate("-(10 - 4) * 3"), Ok(-18));
}

#[test]
fn token_stream_for_long_numbers() {
    assert_eq!(
        tokenize(" 90/ 15 ").unwrap(),
        vec![Token::Num(90), Token::Slash, Token::Num(15)]
    );
}

#[test]
fn division_tree_shape() {
    let e = parse(&tokenize("24/4/2").unwrap()).unwrap();
    assert_eq!(
        e,
        Expr::Div(
            Box::new(Expr::Div(Box::new(Expr::Num(24)), Box::new(Expr::Num(4)))),
            Box::new(Expr::Num(2))
        )
    );
}

#[test]
fn errors_are_preserved() {
    assert_eq!(
        calculate("5 / (10 - 10)"),
        Err(CalcError::Eval(EvalError::DivisionByZero))
    );
    assert_eq!(
        calculate("12 +"),
        Err(CalcError::Parse(ParseError::UnexpectedEnd))
    );
    assert_eq!(
        calculate("12 34"),
        Err(CalcError::Parse(ParseError::UnexpectedToken(Token::Num(
            34
        ))))
    );
    assert_eq!(
        calculate("9223372036854775807 + 1"),
        Err(CalcError::Eval(EvalError::Overflow))
    );
}
