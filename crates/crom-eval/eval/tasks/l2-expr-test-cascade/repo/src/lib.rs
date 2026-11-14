//! Integer arithmetic calculator: lexer -> parser -> evaluator.

pub mod eval;
pub mod lexer;
pub mod parser;

pub use eval::{evaluate, EvalError};
pub use lexer::{tokenize, Token};
pub use parser::{parse, Expr, ParseError};

/// Errors from [`calculate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcError {
    Parse(ParseError),
    Eval(EvalError),
}

/// Tokenize, parse, and evaluate `input`.
pub fn calculate(input: &str) -> Result<i64, CalcError> {
    let tokens = tokenize(input).map_err(CalcError::Parse)?;
    let expr = parse(&tokens).map_err(CalcError::Parse)?;
    evaluate(&expr).map_err(CalcError::Eval)
}
