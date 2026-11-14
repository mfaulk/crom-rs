//! Recursive-descent parser. Grammar:
//!
//! ```text
//! expr   := term (('+' | '-') term)*
//! term   := unary (('*' | '/') unary)*
//! unary  := '-' unary | atom
//! atom   := NUM | '(' expr ')'
//! ```
//!
//! All binary operators are left-associative.

use crate::lexer::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Num(i64),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnexpectedChar(char, usize),
    UnexpectedToken(Token),
    UnexpectedEnd,
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.peek();
        self.pos += 1;
        t
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.term()?;
        match self.peek() {
            Some(Token::Plus) => {
                self.bump();
                Ok(Expr::Add(Box::new(lhs), Box::new(self.expr()?)))
            }
            Some(Token::Minus) => {
                self.bump();
                Ok(Expr::Sub(Box::new(lhs), Box::new(self.expr()?)))
            }
            _ => Ok(lhs),
        }
    }

    fn term(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.unary()?;
        match self.peek() {
            Some(Token::Star) => {
                self.bump();
                Ok(Expr::Mul(Box::new(lhs), Box::new(self.term()?)))
            }
            Some(Token::Slash) => {
                self.bump();
                Ok(Expr::Div(Box::new(lhs), Box::new(self.term()?)))
            }
            _ => Ok(lhs),
        }
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.peek() == Some(Token::Minus) {
            self.bump();
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, ParseError> {
        match self.bump() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::LParen) => {
                let inner = self.expr()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(inner),
                    Some(t) => Err(ParseError::UnexpectedToken(t)),
                    None => Err(ParseError::UnexpectedEnd),
                }
            }
            Some(t) => Err(ParseError::UnexpectedToken(t)),
            None => Err(ParseError::UnexpectedEnd),
        }
    }
}

/// Parse a full token stream into an expression tree.
pub fn parse(tokens: &[Token]) -> Result<Expr, ParseError> {
    let mut p = Parser { tokens, pos: 0 };
    let e = p.expr()?;
    match p.peek() {
        None => Ok(e),
        Some(t) => Err(ParseError::UnexpectedToken(t)),
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::lexer::tokenize;

    fn num(n: i64) -> Box<Expr> {
        Box::new(Expr::Num(n))
    }

    #[test]
    fn precedence() {
        let e = parse(&tokenize("1+2*3").unwrap()).unwrap();
        assert_eq!(e, Expr::Add(num(1), Box::new(Expr::Mul(num(2), num(3)))));
    }

    #[test]
    fn subtraction_is_left_associative() {
        let e = parse(&tokenize("8-4-2").unwrap()).unwrap();
        assert_eq!(e, Expr::Sub(Box::new(Expr::Sub(num(8), num(4))), num(2)));
    }

    #[test]
    fn parses_large_literals() {
        assert_eq!(parse(&tokenize("42").unwrap()), Ok(Expr::Num(42)));
    }

    #[test]
    fn reports_unclosed_paren() {
        assert_eq!(
            parse(&tokenize("(1+2").unwrap()),
            Err(ParseError::UnexpectedEnd)
        );
    }
}
