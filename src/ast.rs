//! Abstract syntax tree for the supported SQL subset.
//!
//! Every node implements `Display`, which prints canonical SQL. The printer
//! adds parentheses only where precedence requires them, and the test suite
//! checks that `parse(print(ast)) == ast` for every query it knows about.

use std::fmt::{self, Display, Formatter};

use crate::lexer::Keyword;

pub type Ident = String;

// ---------------------------------------------------------------------------
// Precedence table (shared by the parser and the printer)
// ---------------------------------------------------------------------------

pub mod prec {
    pub const OR: u8 = 1;
    pub const AND: u8 = 2;
    pub const NOT: u8 = 3;
    /// Comparisons plus IS / LIKE / IN / BETWEEN.
    pub const CMP: u8 = 4;
    pub const CONCAT: u8 = 5;
    pub const ADD: u8 = 6;
    pub const MUL: u8 = 7;
    pub const UNARY: u8 = 8;
    /// Atoms: literals, columns, function calls, parenthesized things.
    pub const ATOM: u8 = 100;
}

// ---------------------------------------------------------------------------
// Statements and queries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Box<Query>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableWithJoins>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(Ident),
    /// `expr [AS alias]`
    Expr { expr: Expr, alias: Option<Ident> },
}

/// One entry of the FROM list: a base relation plus any chained joins.
#[derive(Debug, Clone, PartialEq)]
pub struct TableWithJoins {
    pub relation: TableFactor,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableFactor {
    Table {
        name: Ident,
        alias: Option<TableAlias>,
    },
    Derived {
        subquery: Box<Query>,
        alias: Option<TableAlias>,
    },
}

/// `AS name [(col1, col2, ...)]`
#[derive(Debug, Clone, PartialEq)]
pub struct TableAlias {
    pub name: Ident,
    pub columns: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub relation: TableFactor,
    /// `None` only for CROSS JOIN.
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByExpr {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: Option<bool>,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column {
        table: Option<Ident>,
        name: Ident,
    },
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Query>,
        negated: bool,
    },
    Exists {
        subquery: Box<Query>,
        negated: bool,
    },
    /// Scalar subquery: `(SELECT ...)` used as a value.
    Subquery(Box<Query>),
    Function {
        name: Ident,
        args: FunctionArgs,
        distinct: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    Extract {
        field: DateTimeField,
        expr: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// `count(*)`
    Star,
    List(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// Kept as text so the binder can choose integer vs decimal vs float.
    Number(String),
    String(String),
    Boolean(bool),
    Null,
    /// `DATE '1998-12-01'`
    Date(String),
    /// `INTERVAL '90' DAY`
    Interval {
        value: String,
        unit: DateTimeField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Minus,
    Plus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
}

impl BinaryOp {
    pub fn precedence(self) -> u8 {
        use BinaryOp::*;
        match self {
            Or => prec::OR,
            And => prec::AND,
            Eq | NotEq | Lt | LtEq | Gt | GtEq => prec::CMP,
            Concat => prec::CONCAT,
            Plus | Minus => prec::ADD,
            Multiply | Divide | Modulo => prec::MUL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateTimeField {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "year" => DateTimeField::Year,
            "month" => DateTimeField::Month,
            "day" => DateTimeField::Day,
            "hour" => DateTimeField::Hour,
            "minute" => DateTimeField::Minute,
            "second" => DateTimeField::Second,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Integer,
    BigInt,
    Double,
    Decimal {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    Varchar(Option<u32>),
    Boolean,
    Date,
}

impl Expr {
    /// Binding strength of this node, used to decide where the printer
    /// needs parentheses.
    pub fn precedence(&self) -> u8 {
        match self {
            Expr::Binary { op, .. } => op.precedence(),
            Expr::Unary {
                op: UnaryOp::Not, ..
            } => prec::NOT,
            Expr::Unary { .. } => prec::UNARY,
            Expr::IsNull { .. }
            | Expr::Like { .. }
            | Expr::Between { .. }
            | Expr::InList { .. }
            | Expr::InSubquery { .. } => prec::CMP,
            Expr::Exists { negated: true, .. } => prec::NOT,
            _ => prec::ATOM,
        }
    }

    // Small constructors that keep tests readable.
    pub fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }
    pub fn num(n: &str) -> Expr {
        Expr::Literal(Literal::Number(n.into()))
    }
    pub fn str(s: &str) -> Expr {
        Expr::Literal(Literal::String(s.into()))
    }
    pub fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
        Expr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// Prints an identifier, quoting it only if it would not lex back the same way.
pub struct DisplayIdent<'a>(pub &'a str);

impl Display for DisplayIdent<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = self.0;
        let plain = s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
            && Keyword::lookup(s).is_none();
        if plain {
            write!(f, "{s}")
        } else {
            write!(f, "\"{}\"", s.replace('"', "\"\""))
        }
    }
}

fn comma_list<T: Display>(f: &mut Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

/// Writes `child`, wrapping it in parentheses if it binds more loosely than
/// its parent. Right operands also need parens at *equal* precedence because
/// every binary operator here is left-associative: `a - (b - c)`.
fn child(f: &mut Formatter<'_>, e: &Expr, parent: u8, right_side: bool) -> fmt::Result {
    let p = e.precedence();
    if p < parent || (right_side && p == parent) {
        write!(f, "({e})")
    } else {
        write!(f, "{e}")
    }
}

impl Display for Statement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Query(q) => write!(f, "{q}"),
        }
    }
}

impl Display for Query {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "SELECT ")?;
        if self.distinct {
            write!(f, "DISTINCT ")?;
        }
        comma_list(f, &self.projection)?;
        if !self.from.is_empty() {
            write!(f, " FROM ")?;
            comma_list(f, &self.from)?;
        }
        if let Some(w) = &self.selection {
            write!(f, " WHERE {w}")?;
        }
        if !self.group_by.is_empty() {
            write!(f, " GROUP BY ")?;
            comma_list(f, &self.group_by)?;
        }
        if let Some(h) = &self.having {
            write!(f, " HAVING {h}")?;
        }
        if !self.order_by.is_empty() {
            write!(f, " ORDER BY ")?;
            comma_list(f, &self.order_by)?;
        }
        if let Some(l) = &self.limit {
            write!(f, " LIMIT {l}")?;
        }
        if let Some(o) = &self.offset {
            write!(f, " OFFSET {o}")?;
        }
        Ok(())
    }
}

impl Display for SelectItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => write!(f, "*"),
            SelectItem::QualifiedWildcard(t) => write!(f, "{}.*", DisplayIdent(t)),
            SelectItem::Expr { expr, alias: None } => write!(f, "{expr}"),
            SelectItem::Expr {
                expr,
                alias: Some(a),
            } => write!(f, "{expr} AS {}", DisplayIdent(a)),
        }
    }
}

impl Display for TableWithJoins {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.relation)?;
        for j in &self.joins {
            write!(f, " {j}")?;
        }
        Ok(())
    }
}

impl Display for TableFactor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let alias = match self {
            TableFactor::Table { name, alias } => {
                write!(f, "{}", DisplayIdent(name))?;
                alias
            }
            TableFactor::Derived { subquery, alias } => {
                write!(f, "({subquery})")?;
                alias
            }
        };
        if let Some(a) = alias {
            write!(f, " AS {a}")?;
        }
        Ok(())
    }
}

impl Display for TableAlias {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", DisplayIdent(&self.name))?;
        if !self.columns.is_empty() {
            let cols: Vec<_> = self
                .columns
                .iter()
                .map(|c| DisplayIdent(c).to_string())
                .collect();
            write!(f, " ({})", cols.join(", "))?;
        }
        Ok(())
    }
}

impl Display for Join {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let kw = match self.kind {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
            JoinKind::Cross => "CROSS JOIN",
        };
        write!(f, "{kw} {}", self.relation)?;
        if let Some(on) = &self.on {
            write!(f, " ON {on}")?;
        }
        Ok(())
    }
}

impl Display for OrderByExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if !self.asc {
            write!(f, " DESC")?;
        }
        match self.nulls_first {
            Some(true) => write!(f, " NULLS FIRST"),
            Some(false) => write!(f, " NULLS LAST"),
            None => Ok(()),
        }
    }
}

impl Display for BinaryOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use BinaryOp::*;
        let s = match self {
            Or => "OR",
            And => "AND",
            Eq => "=",
            NotEq => "<>",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            Concat => "||",
            Plus => "+",
            Minus => "-",
            Multiply => "*",
            Divide => "/",
            Modulo => "%",
        };
        write!(f, "{s}")
    }
}

impl Display for DateTimeField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = match self {
            DateTimeField::Year => "YEAR",
            DateTimeField::Month => "MONTH",
            DateTimeField::Day => "DAY",
            DateTimeField::Hour => "HOUR",
            DateTimeField::Minute => "MINUTE",
            DateTimeField::Second => "SECOND",
        };
        write!(f, "{s}")
    }
}

impl Display for DataType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Integer => write!(f, "INTEGER"),
            DataType::BigInt => write!(f, "BIGINT"),
            DataType::Double => write!(f, "DOUBLE"),
            DataType::Decimal {
                precision: None, ..
            } => write!(f, "DECIMAL"),
            DataType::Decimal {
                precision: Some(p),
                scale: None,
            } => write!(f, "DECIMAL({p})"),
            DataType::Decimal {
                precision: Some(p),
                scale: Some(s),
            } => write!(f, "DECIMAL({p}, {s})"),
            DataType::Varchar(None) => write!(f, "VARCHAR"),
            DataType::Varchar(Some(n)) => write!(f, "VARCHAR({n})"),
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Date => write!(f, "DATE"),
        }
    }
}

fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Number(n) => write!(f, "{n}"),
            Literal::String(s) => write!(f, "{}", quote_str(s)),
            Literal::Boolean(true) => write!(f, "TRUE"),
            Literal::Boolean(false) => write!(f, "FALSE"),
            Literal::Null => write!(f, "NULL"),
            Literal::Date(d) => write!(f, "DATE {}", quote_str(d)),
            Literal::Interval { value, unit } => write!(f, "INTERVAL {} {unit}", quote_str(value)),
        }
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let not = |negated: bool| if negated { "NOT " } else { "" };
        match self {
            Expr::Column { table: None, name } => write!(f, "{}", DisplayIdent(name)),
            Expr::Column {
                table: Some(t),
                name,
            } => {
                write!(f, "{}.{}", DisplayIdent(t), DisplayIdent(name))
            }
            Expr::Literal(l) => write!(f, "{l}"),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                write!(f, "NOT ")?;
                child(f, expr, prec::NOT, false)
            }
            Expr::Unary { op, expr } => {
                write!(f, "{}", if *op == UnaryOp::Minus { "-" } else { "+" })?;
                // `- -x` must not print as `--x`, which would lex as a comment.
                if matches!(**expr, Expr::Unary { .. }) {
                    write!(f, "({expr})")
                } else {
                    child(f, expr, prec::UNARY, false)
                }
            }
            Expr::Binary { left, op, right } => {
                let p = op.precedence();
                child(f, left, p, false)?;
                write!(f, " {op} ")?;
                child(f, right, p, true)
            }
            Expr::IsNull { expr, negated } => {
                child(f, expr, prec::CMP, false)?;
                write!(f, " IS {}NULL", not(*negated))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
            } => {
                child(f, expr, prec::CMP, false)?;
                write!(f, " {}LIKE ", not(*negated))?;
                child(f, pattern, prec::CMP, true)
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                child(f, expr, prec::CMP, false)?;
                write!(f, " {}BETWEEN ", not(*negated))?;
                child(f, low, prec::CMP, true)?;
                write!(f, " AND ")?;
                child(f, high, prec::CMP, true)
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                child(f, expr, prec::CMP, false)?;
                write!(f, " {}IN (", not(*negated))?;
                comma_list(f, list)?;
                write!(f, ")")
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                child(f, expr, prec::CMP, false)?;
                write!(f, " {}IN ({subquery})", not(*negated))
            }
            Expr::Exists { subquery, negated } => {
                write!(f, "{}EXISTS ({subquery})", not(*negated))
            }
            Expr::Subquery(q) => write!(f, "({q})"),
            Expr::Function {
                name,
                args,
                distinct,
            } => {
                write!(f, "{}(", DisplayIdent(name))?;
                if *distinct {
                    write!(f, "DISTINCT ")?;
                }
                match args {
                    FunctionArgs::Star => write!(f, "*")?,
                    FunctionArgs::List(list) => comma_list(f, list)?,
                }
                write!(f, ")")
            }
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                write!(f, "CASE")?;
                if let Some(op) = operand {
                    write!(f, " {op}")?;
                }
                for (when, then) in branches {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                if let Some(e) = else_result {
                    write!(f, " ELSE {e}")?;
                }
                write!(f, " END")
            }
            Expr::Cast { expr, data_type } => write!(f, "CAST({expr} AS {data_type})"),
            Expr::Extract { field, expr } => write!(f, "EXTRACT({field} FROM {expr})"),
        }
    }
}
