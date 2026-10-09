//! Quarry: a SQL query engine built from scratch.
//!
//! Pipeline (completed phases in bold):
//! **SQL text -> lexer -> parser -> AST -> binder -> logical plan -> row-at-a-time
//! execution** -> optimizer -> vectorized execution

pub mod agg;
pub mod ast;
pub mod binder;
pub mod bound;
pub mod catalog;
pub mod csv;
pub mod error;
pub mod eval;
pub mod exec;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod tpch;
pub mod types;

pub use binder::{bind, BindError};
pub use error::ParseError;
pub use eval::ExecError;
pub use exec::{execute, QueryResult};
pub use parser::{parse_query, parse_statements};
pub use plan::{plan_query, Plan};
