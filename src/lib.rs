//! Quarry: a SQL query engine built from scratch.
//!
//! Pipeline (current phase in bold):
//! **SQL text -> lexer -> parser -> AST** -> binder -> logical plan
//! -> optimizer -> physical plan -> vectorized execution

pub mod ast;
pub mod error;
pub mod lexer;
pub mod parser;

pub use error::ParseError;
pub use parser::{parse_query, parse_statements};
