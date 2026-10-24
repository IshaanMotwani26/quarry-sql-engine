//! Reading Apache Parquet files, with no dependencies.
//!
//! Supported: flat schemas (no structs, lists or maps), REQUIRED and OPTIONAL
//! columns, and the types Quarry has (booleans, integers, floats, strings,
//! dates, and decimals, which load as DOUBLE).

pub mod meta;
pub mod thrift;

pub use meta::{read_metadata, FileMetaData};
