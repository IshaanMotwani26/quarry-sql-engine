# Quarry

A vectorized SQL query engine written from scratch in Rust, with no dependencies.

```
SQL text → lexer → parser → AST → binder → logical plan → optimizer → physical plan → vectorized execution
           └──────── done ────────┘
```

## Quick start

```bash
cargo test            # 24 tests, including TPC-H round trips and 20k random-AST property cases
cargo run             # REPL: echoes canonical SQL; \ast toggles the syntax tree, \q quits
```

```
quarry> select A+b*2 as total from T where x between 1 and 5 and not exists (select 1 from z);
SELECT a + b * 2 AS total FROM t WHERE x BETWEEN 1 AND 5 AND NOT EXISTS (SELECT 1 FROM z)
```

## Phase 1: SQL front end (complete)

- **Lexer** (`src/lexer.rs`): keywords, case-folded and quoted identifiers, numeric and string literals, `--` and `/* */` comments. Every token carries a line/column span.
- **Parser** (`src/parser.rs`): recursive descent for clauses and Pratt parsing for expressions. It supports SELECT [DISTINCT], comma and explicit joins (INNER/LEFT/RIGHT/FULL/CROSS), derived tables with column aliases, WHERE, GROUP BY, HAVING, ORDER BY (with NULLS FIRST/LAST), LIMIT/OFFSET, scalar/IN/EXISTS subqueries, CASE, CAST, EXTRACT, `DATE`/`INTERVAL` literals, and `SUBSTRING(x FROM a FOR b)`.
- **Printer** (`src/ast.rs`): emits canonical SQL with the minimum parentheses needed.

**How it's tested:** a property test generates random syntax trees, prints them, reparses them, and asserts the result is identical. On its first run it found a real precedence bug: `NOT EXISTS (q) IN (...)` must mean `NOT (EXISTS (q) IN (...))`. The suite also includes TPC-H queries 1, 3, 6, 8, 13, and 22.

## Roadmap

- [x] **1. Front end**: lexer, parser, AST, canonical printer
- [ ] **2. Catalog + binder**: schemas, name resolution, type checking, CSV loading
- [ ] **3. Logical plan + Volcano executor**: scan, filter, project, nested-loop join, sort, limit (first end-to-end queries)
- [ ] **4. Aggregation + hash join**: GROUP BY and hash join; run TPC-H Q1, Q3, Q6
- [ ] **5. Optimizer**: predicate pushdown, projection pruning, constant folding, join reordering
- [ ] **6. Vectorized execution**: columnar batches; benchmark against the Volcano baseline
- [ ] **7. Parquet + Postgres wire protocol**: connect with `psql` and psycopg
- [ ] **8. Evaluation**: TPC-H SF1 benchmarks vs SQLite/DuckDB; differential testing against DuckDB
