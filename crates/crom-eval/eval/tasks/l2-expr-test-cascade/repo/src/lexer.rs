use crate::parser::ParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Num(i64),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

/// Split `input` into tokens. Whitespace is ignored; numbers are non-negative
/// decimal integers (a leading `-` is a separate [`Token::Minus`]).
pub fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((pos, c)) = chars.next() {
        let tok = match c {
            ' ' | '\t' | '\n' => continue,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '(' => Token::LParen,
            ')' => Token::RParen,
            '0'..='9' => {
                let value = i64::from(c as u8 - b'0');
                Token::Num(value)
            }
            other => return Err(ParseError::UnexpectedChar(other, pos)),
        };
        tokens.push(tok);
    }
    Ok(tokens)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn single_digits_and_operators() {
        assert_eq!(
            tokenize("1 + 2*3").unwrap(),
            vec![
                Token::Num(1),
                Token::Plus,
                Token::Num(2),
                Token::Star,
                Token::Num(3)
            ]
        );
    }

    #[test]
    fn multi_digit_numbers() {
        assert_eq!(
            tokenize("12-305").unwrap(),
            vec![Token::Num(12), Token::Minus, Token::Num(305)]
        );
    }

    #[test]
    fn rejects_unknown_characters() {
        assert_eq!(tokenize("1 % 2"), Err(ParseError::UnexpectedChar('%', 2)));
    }
}
