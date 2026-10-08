# Quarry

A vectorized SQL query engine written from scratch in Rust, with no dependencies.

```
SQL text → lexer → parser → AST → binder → logical plan → executor → optimizer → vectorized execution
           └──────────────────────── done ─────────────────────────┘
```

## Quick start

```bash
cargo test            # 105 tests: parser, binder, planner, evaluator, executor, storage, TPC-H
cargo run             # REPL; \? lists commands, \demo loads sample tables
```

```
quarry> \demo
quarry> select e.name, m.name as manager from emp e left join emp m on e.manager_id = m.id order by e.name limit 3;
  name   | manager
---------+---------
 Ada     | NULL
 Barbara | Ken
 Edsger  | NULL
(3 rows)
quarry> select name from dept d where not exists (select 1 from emp e where e.dept_id = d.id);
 name
-------
 Legal
(1 row)
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

## Phase 3b: Volcano executor (complete)

- **Executor** (`src/exec.rs`): pull-based operators (Scan, Filter, Project, nested-loop Join, Distinct, Sort, Limit). Each operator resolves column ids to row positions once, when it's built. The join handles INNER, LEFT, RIGHT, FULL, and CROSS. Sort is stable and honors `NULLS FIRST/LAST`. Limit stops pulling from its input once it has enough rows.
- **Subqueries**: uncorrelated subqueries run once and their results are cached. Correlated ones re-run for each outer row, with that row in scope, including references two or more levels out.
- **Tests**: besides hand-checked queries, a property test compares every join kind against a brute-force reference on 300 random table pairs with duplicate and NULL keys. Deliberately breaking FULL JOIN makes it fail.

Aggregation is planned but not executed yet; that's Phase 4a.

## Roadmap

- [x] **1. Front end**: lexer, parser, AST, canonical printer
- [x] **2. Catalog + binder**: schemas, name resolution, type checking, CSV loading
- [x] **3. Planner + executor**: 3a logical planner and expression evaluator; 3b Volcano executor (first end-to-end queries)
- [ ] **4. Aggregation + hash join**: 4a hash aggregation; 4b hash join; run TPC-H Q1, Q3, Q6
- [ ] **5. Optimizer**: 5a predicate pushdown, projection pruning, constant folding; 5b join reordering, subquery decorrelation
- [ ] **6. Vectorized execution**: 6a columnar batches and vectorized expressions; 6b vectorized operators, benchmarked against the Volcano baseline
- [ ] **7. Interop**: 7a Parquet reader; 7b Postgres wire protocol (connect with `psql` and psycopg)
- [ ] **8. Evaluation**: 8a TPC-H SF1 benchmarks vs SQLite/DuckDB; 8b differential testing against DuckDB
