//! Parquet reading, checked against fixtures written by pyarrow (see
//! tests/data/make_parquet.py).

use std::path::Path;

use quarry::catalog::{Catalog, Table};
use quarry::parquet::meta::{Logical, Physical};
use quarry::parquet::{read_metadata, read_parquet, read_parquet_file};
use quarry::types::{parse_date, Type, Value};
use quarry::{bind, execute, parse_query};

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

fn load(name: &str) -> Table {
    read_parquet("t", &fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The rows of TYPES in make_parquet.py.
fn expected_types_rows() -> Vec<Vec<Value>> {
    use Value::{Boolean as B, Float64 as F, Int64 as I, Null as N};
    let s = |x: &str| Value::Utf8(x.into());
    let d = |x: &str| Value::Date(parse_date(x).unwrap());
    vec![
        vec![
            I(1),
            I(10),
            B(true),
            F(1.5),
            F(0.5),
            s("apple"),
            d("2024-02-29"),
            F(12.34),
            I(1),
        ],
        vec![
            I(2),
            N,
            B(false),
            F(-0.25),
            F(1.25),
            s(""),
            d("1970-01-01"),
            F(-0.05),
            I(-128),
        ],
        vec![I(3), I(-3), N, N, F(-2.0), N, d("1969-12-31"), N, I(127)],
        vec![
            I(4),
            I(2147483647),
            B(true),
            F(1e10),
            N,
            s("naïve café"),
            N,
            F(9999999999.99),
            N,
        ],
        vec![
            I(5),
            I(0),
            B(true),
            F(0.1),
            F(0.0),
            s("apple"),
            d("1998-12-01"),
            F(0.0),
            I(0),
        ],
        vec![
            I(6),
            N,
            B(false),
            F(3.0),
            F(8.0),
            s("a,b|c"),
            d("2000-01-01"),
            F(1.1),
            I(5),
        ],
    ]
}

#[test]
fn reads_every_type_and_null() {
    // Same data five ways: PLAIN values, dictionary-encoded values, version 2
    // data pages, and Snappy-compressed version 1 and 2 pages.
    for name in [
        "plain.parquet",
        "dictionary.parquet",
        "page_v2.parquet",
        "snappy.parquet",
        "snappy_v2.parquet",
    ] {
        let t = load(name);
        assert_eq!(t.row_count(), 6, "{name}");
        let rows: Vec<Vec<Value>> = (0..6).map(|i| t.row(i)).collect();
        assert_eq!(rows, expected_types_rows(), "{name}");
    }
}

#[test]
fn reads_many_pages_and_row_groups() {
    // 3 row groups, small pages, and a dictionary that overflows partway so
    // column k switches from dictionary to PLAIN pages.
    let t = load("pages.parquet");
    assert_eq!(t.row_count(), 1200);
    assert_eq!(load("snappy_pages.parquet"), t);
    for i in 0..1200 {
        let s = if i % 11 == 0 {
            Value::Null
        } else {
            Value::Utf8(format!("s{}", i % 13))
        };
        let expected = vec![
            Value::Int64(i as i64),
            Value::Int64(i as i64 % 7),
            s,
            Value::Boolean(i % 3 == 0),
        ];
        assert_eq!(t.row(i), expected, "row {i}");
    }
}

#[test]
fn rejects_corrupt_pages() {
    let mut file = fixture("plain.parquet");
    // Overwrite the start of the column data, just after the PAR1 header.
    for b in &mut file[4..40] {
        *b = 0xff;
    }
    assert!(read_parquet("t", &file).is_err());
}

#[test]
fn names_unsupported_compression() {
    let err = read_parquet("t", &fixture("zstd.parquet")).unwrap_err();
    assert_eq!(
        err,
        "column id: ZSTD compression is not supported (only Snappy and uncompressed files are)"
    );
}

#[test]
fn corrupt_files_fail_cleanly() {
    // Flip bytes throughout real files: every result must be a table or an
    // error, never a panic.
    for name in [
        "plain.parquet",
        "dictionary.parquet",
        "snappy_pages.parquet",
    ] {
        let original = fixture(name);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..400 {
            let mut file = original.clone();
            for _ in 0..3 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed % file.len() as u64) as usize;
                file[i] ^= (seed >> 32) as u8 | 1;
            }
            let _ = read_parquet("t", &file);
        }
    }
}

fn query(catalog: &Catalog, sql: &str) -> Vec<Vec<Value>> {
    let q = parse_query(sql).unwrap();
    let b = bind(catalog, &q).unwrap();
    execute(catalog, &b).unwrap().rows
}

#[test]
fn queries_a_file_written_by_duckdb() {
    // DuckDB stores `half` as a fixed-length binary DECIMAL and `price` as an
    // INT64 DECIMAL. Expected answers come from running the same queries in
    // DuckDB.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/duckdb.parquet");
    let mut catalog = Catalog::new();
    catalog
        .register(read_parquet_file("items", &path).unwrap())
        .unwrap();
    let d = |x: &str| Value::Date(parse_date(x).unwrap());
    assert_eq!(
        query(
            &catalog,
            "select count(*), count(half), sum(price), min(day), max(day), max(label), sum(half) \
             from items where even"
        ),
        [[
            Value::Int64(500),
            Value::Int64(250),
            Value::Float64(62625.0),
            d("2024-01-03"),
            d("2026-09-27"),
            Value::Utf8("item4".into()),
            Value::Float64(62500.0),
        ]]
    );
    let by_label = query(
        &catalog,
        "select label, count(*), sum(price) from items group by label order by label",
    );
    let sums: Vec<f64> = by_label
        .iter()
        .map(|r| match r[2] {
            Value::Float64(x) => x,
            _ => panic!("{r:?}"),
        })
        .collect();
    assert_eq!(sums, [25125.0, 24925.0, 24975.0, 25025.0, 25075.0]);
    assert!(by_label.iter().all(|r| r[1] == Value::Int64(200)));
}
