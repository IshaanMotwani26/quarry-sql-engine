//! Quarry: a SQL query engine built from scratch.
//!
//! Pipeline (completed phases in bold):
//! **SQL text -> lexer -> parser -> AST -> binder** -> logical plan
//! -> optimizer -> physical plan -> vectorized execution

pub mod ast;
pub mod binder;
pub mod bound;
pub mod catalog;
pub mod csv;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod tpch;
pub mod types;

pub use binder::{bind, BindError};
pub use error::ParseError;
pub use parser::{parse_query, parse_statements};
