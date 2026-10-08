# Quarry

A vectorized SQL query engine written from scratch in Rust, with no dependencies.

```
SQL text → lexer → parser → AST → binder → logical plan → optimizer → physical plan → vectorized execution
           └───────────────────── done ─────────────────────┘
```

## Quick start

```bash
cargo test            # 82 tests: parser, binder, planner, evaluator, storage, TPC-H
cargo run             # REPL; \? lists commands
```

```
quarry> \tpch
registered 8 TPC-H tables (no rows)
quarry> \plan
quarry> select c_name from customer where not exists (select * from orders where o_custkey = c_custkey);
columns: c_name VARCHAR
Query
  scan customer
  filter: NOT EXISTS $sub0
    $sub0:
      Query (correlated on customer.c_custkey)
        scan orders
        filter: (orders.o_custkey = customer.c_custkey)
        ...
```

## Phase 1: SQL front end (complete)

- **Lexer** (`src/lexer.rs`): keywords, case-folded and quoted identifiers, numeric and string literals, `--` and `/* */` comments. Every token carries a line/column span.
- **Parser** (`src/parser.rs`): recursive descent for clauses and Pratt parsing for expressions. It supports SELECT [DISTINCT], comma and explicit joins (INNER/LEFT/RIGHT/FULL/CROSS), derived tables with column aliases, WHERE, GROUP BY, HAVING, ORDER BY (with NULLS FIRST/LAST), LIMIT/OFFSET, scalar/IN/EXISTS subqueries, CASE, CAST, EXTRACT, `DATE`/`INTERVAL` literals, and `SUBSTRING(x FROM a FOR b)`.
- **Printer** (`src/ast.rs`): emits canonical SQL with the minimum parentheses needed.

**How it's tested:** a property test generates random syntax trees, prints them, reparses them, and asserts the result is identical. On its first run it found a real precedence bug: `NOT EXISTS (q) IN (...)` must mean `NOT (EXISTS (q) IN (...))`. The suite also includes TPC-H queries 1, 3, 6, 8, 13, and 22.

## Phase 2: Catalog and binder (complete)

- **Storage** (`src/catalog.rs`): columnar in-memory tables, with one typed vector per column and atomic row inserts.
- **CSV loader** (`src/csv.rs`): hand-written RFC 4180 parser with type inference. It distinguishes NULL from an empty string and reads TPC-H `.tbl` files, reporting errors by line and column.
- **Binder** (`src/binder.rs`): resolves every name to a unique column id, including self-joins, derived tables with column aliases, and correlated subqueries. It type-checks expressions and inserts implicit casts. It enforces SQL's aggregation rules: no aggregates in WHERE, no nested aggregates, and every output either grouped or aggregated. It also resolves ORDER BY by alias, position, or hidden column. Error messages follow Postgres wording.
- **Dates** (`src/types.rs`): Gregorian calendar math with no dependencies, verified by round-tripping every day from 1800 to 2200.

All six TPC-H queries in `src/tpch.rs` bind with the correct output schemas.

## Phase 3a: Logical planner and expression evaluator (complete)

- **Planner** (`src/plan.rs`): turns a bound query into a tree of relational operators (Scan, Filter, Join, Aggregate, Project, Distinct, Sort, Limit) in SQL's logical evaluation order. Operators refer to columns only by id. `\explain` in the REPL prints the plan, including plans for subqueries.
- **Evaluator** (`src/eval.rs`): computes any expression against a row with Postgres semantics. That covers three-valued logic, the NULL behavior of `IN`/`NOT IN`, overflow and division-by-zero errors, month arithmetic that clamps to the end of the month, `LIKE` with escapes, and casts. Correlated column references resolve through a chain of enclosing-row scopes.

## Roadmap

- [x] **1. Front end**: lexer, parser, AST, canonical printer
- [x] **2. Catalog + binder**: schemas, name resolution, type checking, CSV loading
- [ ] **3. Logical plan + Volcano executor** (3a planner + evaluator done): scan, filter, project, nested-loop join, sort, limit (first end-to-end queries)
- [ ] **4. Aggregation + hash join**: GROUP BY and hash join; run TPC-H Q1, Q3, Q6
- [ ] **5. Optimizer**: predicate pushdown, projection pruning, constant folding, join reordering
- [ ] **6. Vectorized execution**: columnar batches; benchmark against the Volcano baseline
- [ ] **7. Parquet + Postgres wire protocol**: connect with `psql` and psycopg
- [ ] **8. Evaluation**: TPC-H SF1 benchmarks vs SQLite/DuckDB; differential testing against DuckDB
