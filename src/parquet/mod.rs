//! Reading Apache Parquet files, with no dependencies.
//!
//! Supported: flat schemas (no structs, lists or maps), REQUIRED and OPTIONAL
//! columns, and the types Quarry has (booleans, integers, floats, strings,
//! dates, and decimals, which load as DOUBLE), PLAIN and dictionary
//! encodings, and version 1 and 2 data pages.

pub mod encoding;
pub mod meta;
pub mod reader;
pub mod thrift;

pub use meta::{read_metadata, FileMetaData};
pub use reader::{read_parquet, read_parquet_file};
