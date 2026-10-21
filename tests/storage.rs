use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, write_csv, CsvOptions};
use quarry::tpch;
use quarry::types::{parse_date, Type, Value};

fn csv(text: &str) -> Result<Table, String> {
    parse_csv("t", text, &CsvOptions::default(), None)
}

#[test]
fn infers_column_types() {
    let t = csv("ID,Price,Day,Flag,Name\n1,2.5,2024-01-31,true,apple\n2,3,2024-02-29,f,banana\n")
        .unwrap();
    let types: Vec<Type> = t.schema.fields.iter().map(|f| f.ty).collect();
    assert_eq!(
        types,
        [
            Type::Int64,
            Type::Float64,
            Type::Date,
            Type::Boolean,
            Type::Utf8
        ]
    );
    assert_eq!(t.schema.fields[0].name, "id", "header names are lowercased");
    assert_eq!(t.row_count(), 2);
    assert_eq!(t.row(1)[1], Value::Float64(3.0));
    assert_eq!(t.row(0)[2], Value::Date(parse_date("2024-01-31").unwrap()));
}

#[test]
fn quoting_and_nulls() {
    let t = csv("a,b\n\"x, \"\"quoted\"\"\",1\n\"\",\n\"multi\nline\",3\n").unwrap();
    assert_eq!(
        t.row(0),
        vec![Value::Utf8("x, \"quoted\"".into()), Value::Int64(1)]
    );
    // Quoted empty is an empty string; unquoted empty is NULL.
    assert_eq!(t.row(1), vec![Value::Utf8(String::new()), Value::Null]);
    assert_eq!(t.row(2)[0], Value::Utf8("multi\nline".into()));
    assert_eq!(
        t.schema.fields[1].ty,
        Type::Int64,
        "NULLs don't affect inference"
    );
}

#[test]
fn quoted_numbers_stay_text() {
    let t = csv("zip\n\"02134\"\n\"10001\"\n").unwrap();
    assert_eq!(t.schema.fields[0].ty, Type::Utf8);
    assert_eq!(t.row(0)[0], Value::Utf8("02134".into()));
}

#[test]
fn crlf_and_blank_lines() {
    let t = csv("a,b\r\n1,2\r\n\r\n3,4\r\n").unwrap();
    assert_eq!(t.row_count(), 2);
    assert_eq!(t.row(1), vec![Value::Int64(3), Value::Int64(4)]);
}

#[test]
fn tpch_tbl_format_with_explicit_schema() {
    let opts = CsvOptions {
        delimiter: '|',
        has_header: false,
    };
    let schema = tpch::schema("nation").unwrap();
    let text = "0|ALGERIA|0| haggle. carefully final deposits|\n1|ARGENTINA|1|al foxes promise|\n";
    let t = parse_csv("nation", text, &opts, Some(&schema)).unwrap();
    assert_eq!(t.row_count(), 2);
    assert_eq!(t.row(1)[1], Value::Utf8("ARGENTINA".into()));
}

#[test]
fn errors_report_line_and_column() {
    let schema = Schema::from_pairs(&[("a", Type::Int64), ("d", Type::Date)]);
    let opts = CsvOptions::default();
    let e = parse_csv(
        "t",
        "a,d\n1,2024-01-01\nx,2024-01-01\n",
        &opts,
        Some(&schema),
    )
    .unwrap_err();
    assert_eq!(e, "line 3, column a: invalid integer `x`");
    let e = parse_csv("t", "a,d\n1,2024-02-30\n", &opts, Some(&schema)).unwrap_err();
    assert!(e.starts_with("line 2, column d: invalid date"), "{e}");
    assert_eq!(
        csv("a,b\n1,2\n3\n").unwrap_err(),
        "line 3: expected 2 fields, found 1"
    );
    assert_eq!(
        csv("a\n\"open\n").unwrap_err(),
        "line 2: unterminated quoted field"
    );
    assert!(csv("a,a\n1,2\n")
        .unwrap_err()
        .contains("duplicate column name"));
}

#[test]
fn table_rows_are_atomic() {
    let mut t = Table::new(
        "t",
        Schema::from_pairs(&[("a", Type::Int64), ("b", Type::Utf8)]),
    )
    .unwrap();
    assert!(t.push_row(vec![Value::Int64(1), Value::Int64(2)]).is_err());
    assert_eq!(
        t.columns[0].len(),
        0,
        "a failed row must not leave a partial write"
    );
    t.push_row(vec![Value::Null, Value::Utf8("x".into())])
        .unwrap();
    assert_eq!(t.row(0), vec![Value::Null, Value::Utf8("x".into())]);
}

#[test]
fn catalog_rejects_duplicates_and_bad_types() {
    let mut c = Catalog::new();
    c.register(Table::new("t", Schema::default()).unwrap())
        .unwrap();
    assert!(c
        .register(Table::new("t", Schema::default()).unwrap())
        .is_err());
    assert!(Table::new("bad", Schema::from_pairs(&[("i", Type::Interval)])).is_err());
}

#[test]
fn column_stats_count_distinct_values_and_nulls() {
    let mut c = Catalog::new();
    c.register(csv("a,b\n1,x\n2,x\n1,\n,y\n").unwrap()).unwrap();
    let a = c.column_stats("t", 0).unwrap();
    assert_eq!((a.distinct, a.nulls), (2, 1));
    let b = c.column_stats("t", 1).unwrap();
    assert_eq!((b.distinct, b.nulls), (2, 1));
    assert_eq!(c.column_stats("t", 5), None);
    assert_eq!(c.column_stats("nope", 0), None);
}

#[test]
fn column_stats_are_refreshed_when_a_table_is_replaced() {
    let mut c = Catalog::new();
    c.register(csv("a\n1\n2\n").unwrap()).unwrap();
    assert_eq!(c.column_stats("t", 0).unwrap().distinct, 2);
    c.register_or_replace(csv("a\n1\n2\n3\n4\n").unwrap());
    assert_eq!(c.column_stats("t", 0).unwrap().distinct, 4);
}

#[test]
fn written_csv_reads_back_identically() {
    let text = "id,name,price,day,ok\n1,\"comma, inside\",2.5,2024-02-29,true\n2,\"say \"\"hi\"\"\",,1999-12-31,\n3,\"\",0.1,,false\n4,\"two\nlines\",-7,2000-01-01,t\n";
    let original = csv(text).unwrap();
    for has_header in [true, false] {
        let opts = CsvOptions {
            delimiter: ',',
            has_header,
        };
        let mut out = Vec::new();
        write_csv(&original, &mut out, &opts).unwrap();
        let written = String::from_utf8(out).unwrap();
        let back = parse_csv("t", &written, &opts, Some(&original.schema)).unwrap();
        assert_eq!(back, original, "round trip changed the table:\n{written}");
    }
}

#[test]
fn csv_export_uses_the_requested_delimiter() {
    let t = csv("a,b\n1,x|y\n").unwrap();
    let mut out = Vec::new();
    write_csv(
        &t,
        &mut out,
        &CsvOptions {
            delimiter: '|',
            has_header: false,
        },
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "1|\"x|y\"\n");
}
