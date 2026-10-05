//! Parser: tokens -> AST.
//!
//! Statements and clauses use plain recursive descent. Expressions use Pratt
//! parsing (precedence climbing): each infix operator has a binding power from
//! `ast::prec`, and `parse_subexpr(min)` keeps consuming operators that bind
//! tighter than `min`. This handles precedence and left-associativity without
//! one grammar function per precedence level.

use crate::ast::*;
use crate::error::{ParseError, Span};
use crate::lexer::{tokenize, Keyword, Token};

type Result<T> = std::result::Result<T, ParseError>;

/// Parse one or more `;`-separated statements.
pub fn parse_statements(sql: &str) -> Result<Vec<Statement>> {
    let mut p = Parser::new(sql)?;
    let mut stmts = Vec::new();
    loop {
        while p.consume(&Token::Semicolon) {}
        if p.peek() == &Token::Eof {
            return Ok(stmts);
        }
        stmts.push(Statement::Query(Box::new(p.parse_query()?)));
        if !p.consume(&Token::Semicolon) && p.peek() != &Token::Eof {
            return Err(p.unexpected("`;` or end of input"));
        }
    }
}

/// Parse exactly one query (a trailing `;` is allowed).
pub fn parse_query(sql: &str) -> Result<Query> {
    let mut p = Parser::new(sql)?;
    let q = p.parse_query()?;
    p.consume(&Token::Semicolon);
    if p.peek() != &Token::Eof {
        return Err(p.unexpected("end of input"));
    }
    Ok(q)
}

pub struct Parser {
    tokens: Vec<(Token, Span)>,
    pos: usize,
}

impl Parser {
    pub fn new(sql: &str) -> Result<Self> {
        Ok(Parser {
            tokens: tokenize(sql)?,
            pos: 0,
        })
    }

    // ---- token helpers ---------------------------------------------------

    fn peek(&self) -> &Token {
        self.peek_nth(0)
    }

    fn peek_nth(&self, n: usize) -> &Token {
        // The token list always ends in Eof, so clamp to it.
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].0
    }

    fn span(&self) -> Span {
        self.tokens[self.pos.min(self.tokens.len() - 1)].1
    }

    fn next(&mut self) -> Token {
        let tok = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    fn consume(&mut self, tok: &Token) -> bool {
        if self.peek() == tok {
            self.next();
            true
        } else {
            false
        }
    }

    fn consume_kw(&mut self, kw: Keyword) -> bool {
        self.consume(&Token::Keyword(kw))
    }

    /// Matches a non-reserved word such as NULLS, FIRST or FOR.
    fn consume_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Token::Ident(s) if s == word) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: Token) -> Result<()> {
        if self.consume(&tok) {
            Ok(())
        } else {
            Err(self.unexpected(&tok.to_string()))
        }
    }

    fn expect_kw(&mut self, kw: Keyword) -> Result<()> {
        if self.consume_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected(kw.as_str()))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        ParseError::new(
            format!("expected {expected}, found {}", self.peek()),
            self.span(),
        )
    }

    fn parse_ident(&mut self) -> Result<Ident> {
        match self.peek().clone() {
            Token::Ident(s) | Token::QuotedIdent(s) => {
                self.next();
                Ok(s)
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    fn comma_separated<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut items = vec![f(self)?];
        while self.consume(&Token::Comma) {
            items.push(f(self)?);
        }
        Ok(items)
    }

    // ---- queries ---------------------------------------------------------

    pub fn parse_query(&mut self) -> Result<Query> {
        self.expect_kw(Keyword::SELECT)?;
        let distinct = self.consume_kw(Keyword::DISTINCT);
        if !distinct {
            self.consume_kw(Keyword::ALL);
        }
        let projection = self.comma_separated(Self::parse_select_item)?;
        let mut q = Query {
            distinct,
            projection,
            ..Default::default()
        };

        if self.consume_kw(Keyword::FROM) {
            q.from = self.comma_separated(Self::parse_table_with_joins)?;
        }
        if self.consume_kw(Keyword::WHERE) {
            q.selection = Some(self.parse_expr()?);
        }
        if self.consume_kw(Keyword::GROUP) {
            self.expect_kw(Keyword::BY)?;
            q.group_by = self.comma_separated(Self::parse_expr)?;
        }
        if self.consume_kw(Keyword::HAVING) {
            q.having = Some(self.parse_expr()?);
        }
        if self.consume_kw(Keyword::ORDER) {
            self.expect_kw(Keyword::BY)?;
            q.order_by = self.comma_separated(Self::parse_order_by_expr)?;
        }
        // LIMIT and OFFSET may come in either order, each at most once.
        loop {
            if q.limit.is_none() && self.consume_kw(Keyword::LIMIT) {
                q.limit = Some(self.parse_expr()?);
            } else if q.offset.is_none() && self.consume_kw(Keyword::OFFSET) {
                q.offset = Some(self.parse_expr()?);
            } else {
                break;
            }
        }
        Ok(q)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.consume(&Token::Star) {
            return Ok(SelectItem::Wildcard);
        }
        // t.*
        if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_))
            && self.peek_nth(1) == &Token::Dot
            && self.peek_nth(2) == &Token::Star
        {
            let table = self.parse_ident()?;
            self.next();
            self.next();
            return Ok(SelectItem::QualifiedWildcard(table));
        }
        let expr = self.parse_expr()?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    /// `AS name` or a bare `name`. Reserved words are separate tokens, so a
    /// following FROM/WHERE/etc. is never mistaken for an alias.
    fn parse_optional_alias(&mut self) -> Result<Option<Ident>> {
        if self.consume_kw(Keyword::AS) {
            return self.parse_ident().map(Some);
        }
        match self.peek() {
            Token::Ident(_) | Token::QuotedIdent(_) => self.parse_ident().map(Some),
            _ => Ok(None),
        }
    }

    fn parse_order_by_expr(&mut self) -> Result<OrderByExpr> {
        let expr = self.parse_expr()?;
        let asc = if self.consume_kw(Keyword::DESC) {
            false
        } else {
            self.consume_kw(Keyword::ASC);
            true
        };
        let nulls_first = if self.consume_word("nulls") {
            if self.consume_word("first") {
                Some(true)
            } else if self.consume_word("last") {
                Some(false)
            } else {
                return Err(self.unexpected("FIRST or LAST"));
            }
        } else {
            None
        };
        Ok(OrderByExpr {
            expr,
            asc,
            nulls_first,
        })
    }

    // ---- FROM clause -----------------------------------------------------

    fn parse_table_with_joins(&mut self) -> Result<TableWithJoins> {
        let relation = self.parse_table_factor()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.consume_kw(Keyword::CROSS) {
                self.expect_kw(Keyword::JOIN)?;
                JoinKind::Cross
            } else if self.consume_kw(Keyword::JOIN) {
                JoinKind::Inner
            } else if self.consume_kw(Keyword::INNER) {
                self.expect_kw(Keyword::JOIN)?;
                JoinKind::Inner
            } else if let Some(kind) = self.parse_outer_join_kind()? {
                kind
            } else {
                break;
            };
            let relation = self.parse_table_factor()?;
            let on = if kind == JoinKind::Cross {
                None
            } else {
                self.expect_kw(Keyword::ON)?;
                Some(self.parse_expr()?)
            };
            joins.push(Join { kind, relation, on });
        }
        Ok(TableWithJoins { relation, joins })
    }

    fn parse_outer_join_kind(&mut self) -> Result<Option<JoinKind>> {
        let kind = match self.peek() {
            Token::Keyword(Keyword::LEFT) => JoinKind::Left,
            Token::Keyword(Keyword::RIGHT) => JoinKind::Right,
            Token::Keyword(Keyword::FULL) => JoinKind::Full,
            _ => return Ok(None),
        };
        self.next();
        self.consume_kw(Keyword::OUTER);
        self.expect_kw(Keyword::JOIN)?;
        Ok(Some(kind))
    }

    fn parse_table_factor(&mut self) -> Result<TableFactor> {
        if self.consume(&Token::LParen) {
            if self.peek() != &Token::Keyword(Keyword::SELECT) {
                return Err(self.unexpected("subquery"));
            }
            let subquery = Box::new(self.parse_query()?);
            self.expect(Token::RParen)?;
            let alias = self.parse_table_alias()?;
            return Ok(TableFactor::Derived { subquery, alias });
        }
        let name = self.parse_ident()?;
        let alias = self.parse_table_alias()?;
        Ok(TableFactor::Table { name, alias })
    }

    fn parse_table_alias(&mut self) -> Result<Option<TableAlias>> {
        let Some(name) = self.parse_optional_alias()? else {
            return Ok(None);
        };
        let columns = if self.consume(&Token::LParen) {
            let cols = self.comma_separated(Self::parse_ident)?;
            self.expect(Token::RParen)?;
            cols
        } else {
            Vec::new()
        };
        Ok(Some(TableAlias { name, columns }))
    }

    // ---- expressions -----------------------------------------------------

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_subexpr(0)
    }

    fn parse_subexpr(&mut self, min_prec: u8) -> Result<Expr> {
        let mut left = self.parse_prefix()?;
        loop {
            let p = self.next_infix_precedence();
            if p <= min_prec {
                return Ok(left);
            }
            left = self.parse_infix(left, p)?;
        }
    }

    fn next_infix_precedence(&self) -> u8 {
        use Keyword as K;
        match self.peek() {
            Token::Keyword(K::OR) => prec::OR,
            Token::Keyword(K::AND) => prec::AND,
            Token::Eq | Token::NotEq | Token::Lt | Token::LtEq | Token::Gt | Token::GtEq => {
                prec::CMP
            }
            Token::Keyword(K::IS | K::LIKE | K::IN | K::BETWEEN) => prec::CMP,
            Token::Keyword(K::NOT)
                if matches!(
                    self.peek_nth(1),
                    Token::Keyword(K::LIKE | K::IN | K::BETWEEN)
                ) =>
            {
                prec::CMP
            }
            Token::Concat => prec::CONCAT,
            Token::Plus | Token::Minus => prec::ADD,
            Token::Star | Token::Slash | Token::Percent => prec::MUL,
            _ => 0,
        }
    }

    fn parse_infix(&mut self, left: Expr, p: u8) -> Result<Expr> {
        let left = Box::new(left);
        let tok = self.next();
        let op = match tok {
            Token::Keyword(Keyword::OR) => Some(BinaryOp::Or),
            Token::Keyword(Keyword::AND) => Some(BinaryOp::And),
            Token::Eq => Some(BinaryOp::Eq),
            Token::NotEq => Some(BinaryOp::NotEq),
            Token::Lt => Some(BinaryOp::Lt),
            Token::LtEq => Some(BinaryOp::LtEq),
            Token::Gt => Some(BinaryOp::Gt),
            Token::GtEq => Some(BinaryOp::GtEq),
            Token::Concat => Some(BinaryOp::Concat),
            Token::Plus => Some(BinaryOp::Plus),
            Token::Minus => Some(BinaryOp::Minus),
            Token::Star => Some(BinaryOp::Multiply),
            Token::Slash => Some(BinaryOp::Divide),
            Token::Percent => Some(BinaryOp::Modulo),
            _ => None,
        };
        if let Some(op) = op {
            // Same min-precedence as our own => left-associative.
            let right = Box::new(self.parse_subexpr(p)?);
            return Ok(Expr::Binary { left, op, right });
        }

        if tok == Token::Keyword(Keyword::IS) {
            let negated = self.consume_kw(Keyword::NOT);
            self.expect_kw(Keyword::NULL)?;
            return Ok(Expr::IsNull {
                expr: left,
                negated,
            });
        }

        let negated = tok == Token::Keyword(Keyword::NOT);
        let tok = if negated { self.next() } else { tok };
        match tok {
            Token::Keyword(Keyword::LIKE) => {
                let pattern = Box::new(self.parse_subexpr(p)?);
                Ok(Expr::Like {
                    expr: left,
                    pattern,
                    negated,
                })
            }
            Token::Keyword(Keyword::BETWEEN) => {
                // Parsing bounds at CMP precedence stops at the AND keyword.
                let low = Box::new(self.parse_subexpr(p)?);
                self.expect_kw(Keyword::AND)?;
                let high = Box::new(self.parse_subexpr(p)?);
                Ok(Expr::Between {
                    expr: left,
                    low,
                    high,
                    negated,
                })
            }
            Token::Keyword(Keyword::IN) => {
                self.expect(Token::LParen)?;
                let result = if self.peek() == &Token::Keyword(Keyword::SELECT) {
                    let subquery = Box::new(self.parse_query()?);
                    Expr::InSubquery {
                        expr: left,
                        subquery,
                        negated,
                    }
                } else {
                    let list = self.comma_separated(Self::parse_expr)?;
                    Expr::InList {
                        expr: left,
                        list,
                        negated,
                    }
                };
                self.expect(Token::RParen)?;
                Ok(result)
            }
            _ => unreachable!("next_infix_precedence only admits known operators"),
        }
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        let span = self.span();
        let tok = self.next();
        match tok {
            Token::Number(n) => Ok(Expr::Literal(Literal::Number(n))),
            Token::String(s) => Ok(Expr::Literal(Literal::String(s))),
            Token::Keyword(Keyword::TRUE) => Ok(Expr::Literal(Literal::Boolean(true))),
            Token::Keyword(Keyword::FALSE) => Ok(Expr::Literal(Literal::Boolean(false))),
            Token::Keyword(Keyword::NULL) => Ok(Expr::Literal(Literal::Null)),

            Token::Keyword(Keyword::NOT) => {
                // Parse the operand at NOT's precedence first, then fold a bare
                // EXISTS into NOT EXISTS. Checking for EXISTS up front would be
                // wrong: `NOT EXISTS (q) IN (..)` means NOT (EXISTS (q) IN (..)).
                match self.parse_subexpr(prec::NOT)? {
                    Expr::Exists {
                        subquery,
                        negated: false,
                    } => Ok(Expr::Exists {
                        subquery,
                        negated: true,
                    }),
                    expr => Ok(Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(expr),
                    }),
                }
            }
            Token::Minus | Token::Plus => {
                let op = if tok == Token::Minus {
                    UnaryOp::Minus
                } else {
                    UnaryOp::Plus
                };
                let expr = Box::new(self.parse_subexpr(prec::UNARY)?);
                Ok(Expr::Unary { op, expr })
            }

            Token::Keyword(Keyword::EXISTS) => self.parse_exists(false),
            Token::Keyword(Keyword::CASE) => self.parse_case(),
            Token::Keyword(Keyword::CAST) => {
                self.expect(Token::LParen)?;
                let expr = Box::new(self.parse_expr()?);
                self.expect_kw(Keyword::AS)?;
                let data_type = self.parse_data_type()?;
                self.expect(Token::RParen)?;
                Ok(Expr::Cast { expr, data_type })
            }
            Token::Keyword(Keyword::EXTRACT) => {
                self.expect(Token::LParen)?;
                let field = self.parse_datetime_field()?;
                self.expect_kw(Keyword::FROM)?;
                let expr = Box::new(self.parse_expr()?);
                self.expect(Token::RParen)?;
                Ok(Expr::Extract { field, expr })
            }

            Token::LParen => {
                let expr = if self.peek() == &Token::Keyword(Keyword::SELECT) {
                    Expr::Subquery(Box::new(self.parse_query()?))
                } else {
                    self.parse_expr()?
                };
                self.expect(Token::RParen)?;
                Ok(expr)
            }

            // DATE 'yyyy-mm-dd' and INTERVAL 'n' unit are contextual, so
            // columns named `date` or `interval` still work.
            Token::Ident(ref w) if w == "date" && matches!(self.peek(), Token::String(_)) => {
                let Token::String(s) = self.next() else {
                    unreachable!()
                };
                Ok(Expr::Literal(Literal::Date(s)))
            }
            Token::Ident(ref w) if w == "interval" && matches!(self.peek(), Token::String(_)) => {
                let Token::String(value) = self.next() else {
                    unreachable!()
                };
                let unit = self.parse_datetime_field()?;
                // Optional precision, e.g. `DAY (3)` as emitted by TPC-H qgen.
                if self.peek() == &Token::LParen && matches!(self.peek_nth(1), Token::Number(_)) {
                    self.next();
                    self.next();
                    self.expect(Token::RParen)?;
                }
                Ok(Expr::Literal(Literal::Interval { value, unit }))
            }

            Token::Ident(name) | Token::QuotedIdent(name) => {
                if self.peek() == &Token::LParen {
                    self.parse_function(name)
                } else if self.consume(&Token::Dot) {
                    let col = self.parse_ident()?;
                    Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    })
                } else {
                    Ok(Expr::Column { table: None, name })
                }
            }

            other => Err(ParseError::new(
                format!("expected expression, found {other}"),
                span,
            )),
        }
    }

    fn parse_exists(&mut self, negated: bool) -> Result<Expr> {
        self.expect(Token::LParen)?;
        let subquery = Box::new(self.parse_query()?);
        self.expect(Token::RParen)?;
        Ok(Expr::Exists { subquery, negated })
    }

    fn parse_function(&mut self, name: Ident) -> Result<Expr> {
        self.expect(Token::LParen)?;
        if self.consume(&Token::Star) {
            self.expect(Token::RParen)?;
            return Ok(Expr::Function {
                name,
                args: FunctionArgs::Star,
                distinct: false,
            });
        }
        if self.consume(&Token::RParen) {
            return Ok(Expr::Function {
                name,
                args: FunctionArgs::List(vec![]),
                distinct: false,
            });
        }
        let distinct = self.consume_kw(Keyword::DISTINCT);
        let first = self.parse_expr()?;
        let mut args = vec![first];

        // SQL-standard SUBSTRING(s FROM start [FOR len]) normalizes to
        // substring(s, start[, len]).
        if name == "substring" && self.consume_kw(Keyword::FROM) {
            args.push(self.parse_expr()?);
            if self.consume_word("for") {
                args.push(self.parse_expr()?);
            }
        } else {
            while self.consume(&Token::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(Token::RParen)?;
        Ok(Expr::Function {
            name,
            args: FunctionArgs::List(args),
            distinct,
        })
    }

    fn parse_case(&mut self) -> Result<Expr> {
        let operand = if self.peek() == &Token::Keyword(Keyword::WHEN) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut branches = Vec::new();
        while self.consume_kw(Keyword::WHEN) {
            let when = self.parse_expr()?;
            self.expect_kw(Keyword::THEN)?;
            let then = self.parse_expr()?;
            branches.push((when, then));
        }
        if branches.is_empty() {
            return Err(self.unexpected("WHEN"));
        }
        let else_result = if self.consume_kw(Keyword::ELSE) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_kw(Keyword::END)?;
        Ok(Expr::Case {
            operand,
            branches,
            else_result,
        })
    }

    fn parse_datetime_field(&mut self) -> Result<DateTimeField> {
        let span = self.span();
        let name = self.parse_ident()?;
        DateTimeField::from_name(&name)
            .ok_or_else(|| ParseError::new(format!("unknown date/time field `{name}`"), span))
    }

    fn parse_u32(&mut self) -> Result<u32> {
        let span = self.span();
        match self.next() {
            Token::Number(n) => n.parse().map_err(|_| {
                ParseError::new(format!("expected a positive integer, found {n}"), span)
            }),
            other => Err(ParseError::new(
                format!("expected a positive integer, found {other}"),
                span,
            )),
        }
    }

    /// Optional `(a)` or `(a, b)` after a type name.
    fn parse_type_params(&mut self) -> Result<(Option<u32>, Option<u32>)> {
        if !self.consume(&Token::LParen) {
            return Ok((None, None));
        }
        let a = self.parse_u32()?;
        let b = if self.consume(&Token::Comma) {
            Some(self.parse_u32()?)
        } else {
            None
        };
        self.expect(Token::RParen)?;
        Ok((Some(a), b))
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        let span = self.span();
        let name = self.parse_ident()?;
        let dt = match name.as_str() {
            "int" | "integer" => DataType::Integer,
            "bigint" => DataType::BigInt,
            "double" => {
                self.consume_word("precision");
                DataType::Double
            }
            "float" | "real" => DataType::Double,
            "decimal" | "numeric" => {
                let (precision, scale) = self.parse_type_params()?;
                DataType::Decimal { precision, scale }
            }
            "varchar" | "text" | "char" => match self.parse_type_params()? {
                (n, None) => DataType::Varchar(n),
                _ => return Err(ParseError::new("VARCHAR takes one parameter", span)),
            },
            "boolean" | "bool" => DataType::Boolean,
            "date" => DataType::Date,
            other => {
                return Err(ParseError::new(
                    format!("unknown data type `{other}`"),
                    span,
                ))
            }
        };
        Ok(dt)
    }
}
