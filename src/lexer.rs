//! Lexer: turns raw SQL text into a flat list of tokens, each tagged with
//! its source position so the parser can report precise errors.

use std::fmt;

use crate::error::{ParseError, Span};

/// Generates the `Keyword` enum plus string conversions from one list,
/// so adding a reserved word is a one-line change.
macro_rules! keywords {
    ($($kw:ident),* $(,)?) => {
        /// Reserved words. These can never be bare identifiers; quote them
        /// ("select") to use them as names.
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Keyword { $($kw),* }

        impl Keyword {
            pub fn lookup(word: &str) -> Option<Keyword> {
                match word.to_ascii_uppercase().as_str() {
                    $(stringify!($kw) => Some(Keyword::$kw),)*
                    _ => None,
                }
            }

            pub fn as_str(&self) -> &'static str {
                match self { $(Keyword::$kw => stringify!($kw)),* }
            }
        }
    };
}

keywords!(
    ALL, AND, AS, ASC, BETWEEN, BY, CASE, CAST, CROSS, DESC, DISTINCT, ELSE, END, EXISTS, EXTRACT,
    FALSE, FROM, FULL, GROUP, HAVING, IN, INNER, IS, JOIN, LEFT, LIKE, LIMIT, NOT, NULL, OFFSET,
    ON, OR, ORDER, OUTER, RIGHT, SELECT, THEN, TRUE, WHEN, WHERE,
);

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Keyword(Keyword),
    /// Unquoted identifier, normalized to lowercase (Postgres semantics).
    Ident(String),
    /// "Quoted" identifier, case preserved.
    QuotedIdent(String),
    /// Numeric literal kept as text; typing happens later in the binder.
    Number(String),
    /// 'string' literal with '' escapes already resolved.
    String(String),
    Comma,
    Dot,
    LParen,
    RParen,
    Semicolon,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Keyword(k) => write!(f, "{}", k.as_str()),
            Token::Ident(s) => write!(f, "identifier `{s}`"),
            Token::QuotedIdent(s) => write!(f, "identifier \"{s}\""),
            Token::Number(s) => write!(f, "number {s}"),
            Token::String(s) => write!(f, "string '{s}'"),
            Token::Comma => write!(f, "`,`"),
            Token::Dot => write!(f, "`.`"),
            Token::LParen => write!(f, "`(`"),
            Token::RParen => write!(f, "`)`"),
            Token::Semicolon => write!(f, "`;`"),
            Token::Star => write!(f, "`*`"),
            Token::Plus => write!(f, "`+`"),
            Token::Minus => write!(f, "`-`"),
            Token::Slash => write!(f, "`/`"),
            Token::Percent => write!(f, "`%`"),
            Token::Eq => write!(f, "`=`"),
            Token::NotEq => write!(f, "`<>`"),
            Token::Lt => write!(f, "`<`"),
            Token::LtEq => write!(f, "`<=`"),
            Token::Gt => write!(f, "`>`"),
            Token::GtEq => write!(f, "`>=`"),
            Token::Concat => write!(f, "`||`"),
            Token::Eof => write!(f, "end of input"),
        }
    }
}

/// Tokenize a full SQL string. The returned vector always ends with `Token::Eof`.
pub fn tokenize(input: &str) -> Result<Vec<(Token, Span)>, ParseError> {
    let mut lexer = Lexer {
        chars: input.chars().collect(),
        pos: 0,
        line: 1,
        col: 1,
    };
    let mut tokens = Vec::new();
    loop {
        let (tok, span) = lexer.next_token()?;
        let done = tok == Token::Eof;
        tokens.push((tok, span));
        if done {
            return Ok(tokens);
        }
    }
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn span(&self) -> Span {
        Span {
            line: self.line,
            col: self.col,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), ParseError> {
        loop {
            match (self.peek(), self.peek_at(1)) {
                (Some(c), _) if c.is_whitespace() => {
                    self.bump();
                }
                // -- line comment
                (Some('-'), Some('-')) => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                // /* block comment */
                (Some('/'), Some('*')) => {
                    let start = self.span();
                    self.bump();
                    self.bump();
                    loop {
                        match (self.peek(), self.peek_at(1)) {
                            (Some('*'), Some('/')) => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            (Some(_), _) => {
                                self.bump();
                            }
                            (None, _) => {
                                return Err(ParseError::new("unterminated block comment", start))
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self) -> Result<(Token, Span), ParseError> {
        self.skip_whitespace_and_comments()?;
        let span = self.span();
        let Some(c) = self.peek() else {
            return Ok((Token::Eof, span));
        };

        let tok = match c {
            c if c.is_ascii_alphabetic() || c == '_' => self.word(),
            c if c.is_ascii_digit() => self.number(span)?,
            '.' if self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) => self.number(span)?,
            '\'' => Token::String(self.quoted('\'', span, "string literal")?),
            '"' => Token::QuotedIdent(self.quoted('"', span, "quoted identifier")?),
            _ => self.symbol(c, span)?,
        };
        Ok((tok, span))
    }

    fn word(&mut self) -> Token {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        match Keyword::lookup(&s) {
            Some(kw) => Token::Keyword(kw),
            None => Token::Ident(s.to_ascii_lowercase()),
        }
    }

    fn take_digits(&mut self, s: &mut String) {
        while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
            s.push(c);
            self.bump();
        }
    }

    fn number(&mut self, span: Span) -> Result<Token, ParseError> {
        let mut s = String::new();
        self.take_digits(&mut s);
        if self.peek() == Some('.') {
            s.push('.');
            self.bump();
            self.take_digits(&mut s);
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign = self.peek_at(1);
            let has_exp_digits = match sign {
                Some('+' | '-') => self.peek_at(2).is_some_and(|d| d.is_ascii_digit()),
                Some(d) => d.is_ascii_digit(),
                None => false,
            };
            if !has_exp_digits {
                return Err(ParseError::new(
                    "malformed exponent in numeric literal",
                    span,
                ));
            }
            s.push('e');
            self.bump();
            if let Some(sign @ ('+' | '-')) = self.peek() {
                s.push(sign);
                self.bump();
            }
            self.take_digits(&mut s);
        }
        // Reject things like `123abc`, which are almost always typos.
        if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return Err(ParseError::new(
                format!(
                    "invalid character `{}` after numeric literal",
                    self.peek().unwrap()
                ),
                self.span(),
            ));
        }
        Ok(Token::Number(s))
    }

    /// Reads a delimited token where a doubled delimiter is an escape ('' or "").
    fn quoted(&mut self, delim: char, span: Span, what: &str) -> Result<String, ParseError> {
        self.bump(); // opening delimiter
        let mut s = String::new();
        loop {
            match self.bump() {
                Some(c) if c == delim => {
                    if self.peek() == Some(delim) {
                        self.bump();
                        s.push(delim);
                    } else {
                        return Ok(s);
                    }
                }
                Some(c) => s.push(c),
                None => return Err(ParseError::new(format!("unterminated {what}"), span)),
            }
        }
    }

    fn symbol(&mut self, c: char, span: Span) -> Result<Token, ParseError> {
        self.bump();
        let tok = match c {
            ',' => Token::Comma,
            '.' => Token::Dot,
            '(' => Token::LParen,
            ')' => Token::RParen,
            ';' => Token::Semicolon,
            '*' => Token::Star,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '=' => Token::Eq,
            '<' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Token::LtEq
                }
                Some('>') => {
                    self.bump();
                    Token::NotEq
                }
                _ => Token::Lt,
            },
            '>' => {
                if self.peek() == Some('=') {
                    self.bump();
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }
            '!' if self.peek() == Some('=') => {
                self.bump();
                Token::NotEq
            }
            '|' if self.peek() == Some('|') => {
                self.bump();
                Token::Concat
            }
            other => {
                return Err(ParseError::new(
                    format!("unexpected character `{other}`"),
                    span,
                ))
            }
        };
        Ok(tok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql).unwrap().into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            toks("SELECT Foo FROM \"Bar\""),
            vec![
                Token::Keyword(Keyword::SELECT),
                Token::Ident("foo".into()),
                Token::Keyword(Keyword::FROM),
                Token::QuotedIdent("Bar".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn numbers_strings_operators() {
        assert_eq!(
            toks("1.5e-3 'it''s' <> <= != ||"),
            vec![
                Token::Number("1.5e-3".into()),
                Token::String("it's".into()),
                Token::NotEq,
                Token::LtEq,
                Token::NotEq,
                Token::Concat,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            toks("1 -- hi\n /* multi\nline */ 2"),
            vec![
                Token::Number("1".into()),
                Token::Number("2".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn spans_track_lines() {
        let t = tokenize("SELECT\n  x").unwrap();
        assert_eq!(t[1].1, Span { line: 2, col: 3 });
    }

    #[test]
    fn errors() {
        assert!(tokenize("'abc")
            .unwrap_err()
            .message
            .contains("unterminated string"));
        assert!(tokenize("12abc").is_err());
        assert!(tokenize("a ? b").is_err());
    }
}
