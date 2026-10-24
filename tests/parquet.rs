//! Parquet reading, checked against fixtures written by pyarrow (see
//! tests/data/make_parquet.py).

use quarry::parquet::meta::{Logical, Physical};
use quarry::parquet::read_metadata;
use quarry::types::Type;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn reads_the_footer() {
    let meta = read_metadata(&fixture("plain.parquet")).unwrap();
    assert_eq!(meta.num_rows, 6);
    assert_eq!(meta.row_groups.len(), 1);
    assert!(meta.created_by.unwrap().starts_with("parquet-cpp"));

    let names: Vec<&str> = meta.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["id", "qty", "flag", "price", "ratio", "name", "day", "amount", "tiny"]
    );
    let id = &meta.columns[0];
    assert_eq!((id.physical, id.optional), (Physical::Int64, false));
    assert!(meta.columns[1].optional);
    assert_eq!(meta.columns[5].logical, Logical::String);
    assert_eq!(meta.columns[7].logical, Logical::Decimal { scale: 2 });
    assert_eq!(
        meta.columns[8].logical,
        Logical::Integer {
            bits: 8,
            signed: true
        }
    );
}

#[test]
fn maps_parquet_types_to_quarry_types() {
    let schema = read_metadata(&fixture("plain.parquet"))
        .unwrap()
        .schema()
        .unwrap();
    let types: Vec<Type> = schema.fields.iter().map(|f| f.ty).collect();
    assert_eq!(
        types,
        [
            Type::Int64,
            Type::Int64,
            Type::Boolean,
            Type::Float64,
            Type::Float64,
            Type::Utf8,
            Type::Date,
            Type::Float64,
            Type::Int64
        ]
    );
}

#[test]
fn row_groups_point_inside_the_file() {
    let file = fixture("pages.parquet");
    let meta = read_metadata(&file).unwrap();
    assert_eq!(meta.row_groups.len(), 3);
    let total: i64 = meta.row_groups.iter().map(|rg| rg.num_rows).sum();
    assert_eq!(total, meta.num_rows);
    for rg in &meta.row_groups {
        for chunk in &rg.columns {
            assert_eq!(chunk.num_values, rg.num_rows);
            assert!(chunk.byte_range(file.len()).is_ok());
        }
    }
}

#[test]
fn rejects_files_it_cannot_read() {
    assert!(read_metadata(b"hello, world")
        .unwrap_err()
        .contains("not a Parquet file"));
    let mut truncated = fixture("plain.parquet");
    truncated.truncate(truncated.len() - 1);
    assert!(read_metadata(&truncated).is_err());

    let err = read_metadata(&fixture("nested.parquet")).unwrap_err();
    assert!(err.contains("nested columns"), "{err}");
    let err = read_metadata(&fixture("timestamp.parquet"))
        .unwrap()
        .schema()
        .unwrap_err();
    assert_eq!(err, "column t: TIMESTAMP values are not supported");
}
