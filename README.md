# Quarry

A SQL query engine written from scratch in Rust, with no dependencies. It parses, binds, optimizes, and executes analytical SQL, and runs six TPC-H queries at scale factor 1 (6 million rows) with the same answers as SQLite and DuckDB.

```
SQL text → lexer → parser → binder → logical planner → optimizer → executor → results
```

The optimizer pushes filters down, prunes unused columns, folds constants, turns `EXISTS`/`IN` subqueries into semi and anti joins, and orders joins by estimated cost from table statistics. The executor runs hash joins, hash aggregation, sorting, and correlated subqueries over columnar in-memory tables, loaded from CSV or Parquet files. A built-in Postgres wire protocol server lets `psql` and drivers like psycopg query it.

## Results

`quarry-bench` runs TPC-H queries 1, 3, 6, 8, 13, and 22 in Quarry, SQLite 3.54, and DuckDB 1.5, all in memory, and compares every row of every answer.

**Correctness:** all three engines return identical results for all six queries at scale factors 0.01, 0.05, 0.1, and 1 (6 million lineitem rows), in every setup below.

**Speed at scale factor 1.** Times are best of 3 on a 12-thread Windows PC and will vary by machine. Quarry and SQLite are single-threaded; DuckDB is shown both ways.

| Query | Quarry | SQLite, + foreign-key indexes | DuckDB, 1 thread | DuckDB, 12 threads |
|---|---|---|---|---|
| Q1 | 4,398 ms | 4,417 ms (even) | 185 ms | 51 ms |
| Q3 | 1,533 ms | 694 ms (SQLite 2.2× faster) | 51 ms | 39 ms |
| Q6 | 908 ms | 396 ms (SQLite 2.3× faster) | 28 ms | 10 ms |
| Q8 | 2,228 ms | 2,622 ms (Quarry 1.2× faster) | 37 ms | 30 ms |
| Q13 | 2,869 ms | 1,494 ms (SQLite 1.9× faster) | 166 ms | 122 ms |
| Q22 | 689 ms | 61 ms (SQLite 11× faster) | 22 ms | 31 ms |

**At scale factor 0.1**, SQLite also runs with primary keys only (above 0.1 that setup is skipped: its Q22 plan grows with the square of the data):

| Query | Quarry | SQLite, primary keys only | SQLite, + foreign-key indexes | DuckDB, 1 thread |
|---|---|---|---|---|
| Q1 | 403 ms | 345 ms | 379 ms | 17 ms |
| Q3 | 122 ms | 29 ms | 42 ms | 6 ms |
| Q6 | 75 ms | 44 ms | 48 ms | 3 ms |
| Q8 | 168 ms | 35 ms | 9 ms | 6 ms |
| Q13 | 189 ms | 729 ms | 73 ms | 15 ms |
| Q22 | 41 ms | 3,665 ms | 3 ms | 5 ms |

What the numbers show:

- **DuckDB is 17–60× faster than Quarry on one thread.** That gap is vectorized execution: DuckDB processes columns in batches of 2,048 values with tight loops, while Quarry interprets one row at a time and copies a tagged value for every cell. It is the clearest next step for Quarry (see [Future work](#future-work)).
- **Decorrelation and hash joins beat a missing index.** Against SQLite with only primary keys, Quarry is 89× faster on Q22, because it turns the correlated `NOT EXISTS` into an anti join while SQLite re-runs the subquery for every customer, and 3.9× faster on Q13, where SQLite has no index for the outer join and Quarry builds a hash table.
- **Where SQLite has an index, SQLite wins.** With foreign-key indexes it looks rows up in B-trees, while Quarry, which has no indexes, builds a hash table for every join. SQLite's bytecode VM also beats Quarry's interpreter on Q6, a single-table scan.
- **Quarry's hash joins scale more predictably.** From scale 0.1 to 1, Quarry's Q8 time grows 13× with 10× the data. SQLite's grows about 300× as its eight-table join plan changes, so at scale 1 Quarry is 1.2× faster on Q8 and even on Q1.

Reproduce with `cargo run --release --bin quarry-bench -- 0.1 1 --runs 3`. The comparisons need `sqlite3` and `duckdb` on your PATH (Windows: `winget install SQLite.SQLite DuckDB.cli`, then open a new terminal); either is skipped if missing. DuckDB and SQLite report times to the millisecond.

### How correctness is checked

248 Rust tests, run in CI with `clippy -D warnings` and `rustfmt`:

- **Property tests**: random syntax trees must print and reparse identically.
- **Differential tests**: every query must return the same result with the optimizer on and off, and with hash and nested-loop joins (1,200 random queries plus all six TPC-H queries); semi and anti joins are checked against `EXISTS` on random tables.
- **Reference implementations**: TPC-H Q1, Q3, and Q6 are recomputed directly in Rust and compared.
- **Mutation testing**: deliberately planted bugs (wrong outer-join pushdown, NULL keys that match, a dropped join residual, and others) must each fail at least one test. This found two weak tests, which were strengthened, and one unnecessary optimizer restriction, which was removed.
- **SQLite and DuckDB**: the benchmark fails if any answer differs from either.
- **Differential testing against DuckDB** (`tests/difftest/duckdb_diff.py`, also run in CI): random queries over random tables full of NULLs, duplicates, and empty strings, with outer joins, correlated `EXISTS`/`IN`/scalar subqueries, `NOT IN`, grouping, `HAVING`, `DISTINCT`, and `LIMIT`, must give the same answers in both engines. It found a real bug: `NULL IN` an empty subquery returned NULL instead of FALSE, which made `NOT IN` drop rows. After the fix, about 20,000 queries agree.
- **Parquet fixtures** written by pyarrow and DuckDB, plus thousands of randomly corrupted files that must fail with an error, never a crash.

## Quick start

```bash
cargo test                               # 248 tests
cargo run --release                      # REPL; \? lists commands, \demo loads sample tables
cargo run --release --bin quarry-bench   # TPC-H benchmark against SQLite and DuckDB
cargo run --release --bin quarry-server -- --tpch 0.01   # Postgres-compatible server on port 5433
python tests/difftest/duckdb_diff.py     # random queries vs DuckDB (pip install duckdb "psycopg[binary]")
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
quarry> select d.name, count(e.id) as headcount, avg(e.salary) from dept d left join emp e on e.dept_id = d.id group by d.name order by headcount desc;
    name     | headcount |        avg
-------------+-----------+--------------------
 Engineering |         3 | 165666.66666666666
 Sales       |         2 |             101500
 Research    |         1 |             160000
 Legal       |         0 |               NULL
(4 rows)
quarry> \tpch 0.01
generated TPC-H at scale 0.01 (59597 lineitem rows) in 0.1 s
quarry> \explain
quarry> \q3
Limit 10
  Sort revenue DESC NULLS FIRST, o_orderdate ASC NULLS LAST
    Project l_orderkey, revenue := sum(l_extendedprice * (1 - l_discount)), o_orderdate, o_shippriority
      Aggregate keys=[lineitem.l_orderkey, orders.o_orderdate, orders.o_shippriority] aggs=[...]
        HashJoin Inner keys=[lineitem.l_orderkey = orders.o_orderkey]
          Filter (lineitem.l_shipdate > DATE '1995-03-15')
            Scan lineitem [4/16 columns: l_orderkey, l_extendedprice, l_discount, l_shipdate]
          HashJoin Inner keys=[orders.o_custkey = customer.c_custkey]
            Filter (orders.o_orderdate < DATE '1995-03-15')
              Scan orders [4/9 columns: o_orderkey, o_custkey, o_orderdate, o_shippriority]
            Filter (customer.c_mktsegment = 'BUILDING')
              Scan customer [2/8 columns: c_custkey, c_mktsegment]
 l_orderkey |      revenue       | o_orderdate | o_shippriority
------------+--------------------+-------------+----------------
      13166 |        337532.7514 | 1995-02-14  |              0
...
```

## How it was built

Quarry was built in phases, each ending with passing tests and CI.

### Phase 1: SQL front end

- **Lexer** (`src/lexer.rs`): keywords, case-folded and quoted identifiers, numeric and string literals, `--` and `/* */` comments. Every token carries a line/column span.
- **Parser** (`src/parser.rs`): recursive descent for clauses and Pratt parsing for expressions. It supports SELECT [DISTINCT], comma and explicit joins (INNER/LEFT/RIGHT/FULL/CROSS), derived tables with column aliases, WHERE, GROUP BY, HAVING, ORDER BY (with NULLS FIRST/LAST), LIMIT/OFFSET, scalar/IN/EXISTS subqueries, CASE, CAST, EXTRACT, `DATE`/`INTERVAL` literals, and `SUBSTRING(x FROM a FOR b)`.
- **Printer** (`src/ast.rs`): emits canonical SQL with the minimum parentheses needed.

**How it's tested:** a property test generates random syntax trees, prints them, reparses them, and asserts the result is identical. On its first run it found a real precedence bug: `NOT EXISTS (q) IN (...)` must mean `NOT (EXISTS (q) IN (...))`. The suite also includes TPC-H queries 1, 3, 6, 8, 13, and 22.

### Phase 2: Catalog and binder

- **Storage** (`src/catalog.rs`): columnar in-memory tables, with one typed vector per column and atomic row inserts.
- **CSV loader** (`src/csv.rs`): hand-written RFC 4180 parser with type inference. It distinguishes NULL from an empty string and reads TPC-H `.tbl` files, reporting errors by line and column.
- **Binder** (`src/binder.rs`): resolves every name to a unique column id, including self-joins, derived tables with column aliases, and correlated subqueries. It type-checks expressions and inserts implicit casts. It enforces SQL's aggregation rules: no aggregates in WHERE, no nested aggregates, and every output either grouped or aggregated. It also resolves ORDER BY by alias, position, or hidden column. Error messages follow Postgres wording.
- **Dates** (`src/types.rs`): Gregorian calendar math with no dependencies, verified by round-tripping every day from 1800 to 2200.

All six TPC-H queries in `src/tpch.rs` bind with the correct output schemas.

### Phase 3a: Logical planner and expression evaluator

- **Planner** (`src/plan.rs`): turns a bound query into a tree of relational operators (Scan, Filter, Join, Aggregate, Project, Distinct, Sort, Limit) in SQL's logical evaluation order. Operators refer to columns only by id. `\explain` in the REPL prints the plan, including plans for subqueries.
- **Evaluator** (`src/eval.rs`): computes any expression against a row with Postgres semantics. That covers three-valued logic, the NULL behavior of `IN`/`NOT IN`, overflow and division-by-zero errors, month arithmetic that clamps to the end of the month, `LIKE` with escapes, and casts. Correlated column references resolve through a chain of enclosing-row scopes.

### Phase 3b: Volcano executor

- **Executor** (`src/exec.rs`): pull-based operators (Scan, Filter, Project, nested-loop Join, Distinct, Sort, Limit). Each operator resolves column ids to row positions once, when it's built. The join handles INNER, LEFT, RIGHT, FULL, and CROSS. Sort is stable and honors `NULLS FIRST/LAST`. Limit stops pulling from its input once it has enough rows.
- **Subqueries**: uncorrelated subqueries run once and their results are cached. Correlated ones re-run for each outer row, with that row in scope, including references two or more levels out.
- **Tests**: besides hand-checked queries, a property test compares every join kind against a brute-force reference on 300 random table pairs with duplicate and NULL keys. Deliberately breaking FULL JOIN makes it fail.

### Phase 4a: Hash aggregation

- **HashAggregate operator** (`src/exec.rs`): groups rows by their GROUP BY key values in a hash table. Groups come out in first-seen order, so results are deterministic. With no GROUP BY there is always exactly one group, so `count(*)` over an empty table is `0`, not zero rows.
- **Accumulators** (`src/agg.rs`): `count(*)`, `count`, `sum`, `avg`, `min`, `max`, and their `DISTINCT` forms, with Postgres NULL semantics. Integer `sum` errors on overflow. Floating-point `sum` and `avg` use Neumaier compensated summation, so ten `0.1`s sum to exactly `1.0` (plain `+=` gives `0.9999999999999999`).
- **Works with everything before it**: HAVING (with or without GROUP BY, including subqueries), grouping by expressions, aggregates over joins and derived tables, and correlated subqueries that aggregate once per outer row.
- **Tests**: TPC-H Q1 and Q6 run on 5,000 generated lineitem rows and are checked against a reference implementation written directly in Rust. Injecting bugs into `avg` or NULL handling fails 7 and 5 tests respectively.

### Phase 4b: Hash join and TPC-H data

- **HashJoin operator** (`src/exec.rs`): builds a hash table on the right input's join keys and probes it with each left row, instead of comparing every pair of rows. It supports INNER, LEFT, RIGHT, and FULL joins, multi-column and expression keys, and non-equality conditions alongside the keys. NULL keys never match. Matches come out in the same order as the nested-loop join's, so the two give bit-identical results.
- **Join condition placement** (`src/plan.rs`): `FROM a, b WHERE a.x = b.y` is planned as a hash join instead of a cross product plus a filter. WHERE conditions that span both sides of an inner or comma join move into the lowest join that covers them. Conditions are never moved into outer joins, where that would change the result.
- **TPC-H data generator** (`src/tpch.rs`): `\tpch 0.01` in the REPL generates all eight tables, deterministically, following the spec's sizes, key relationships, and value rules. `\q1` … `\q22` run the queries, and `\hashjoin` switches hash joins off for comparison.
- **Tests**: all six TPC-H queries return identical results with hash and nested-loop joins. Q3 is checked against a reference implementation, and a randomized test covers every join kind with multi-key conditions and residuals. Breaking NULL-key handling fails 8 tests. The generator's own integrity test found that the spec's partsupp formula repeats suppliers below scale 0.023, which would duplicate primary keys; the generator now handles small scales.

**Q3, hash join vs nested loop** (release build):

| Scale | Lineitem rows | Hash join | Nested loop | Speedup |
|---|---|---|---|---|
| 0.001 | 6k | 24 ms | 2.4 s | ~100× |
| 0.002 | 12k | 43 ms | 10.0 s | ~230× |
| 0.005 | 30k | 133 ms | 63.3 s | ~475× |

Doubling the data roughly doubles the hash join's time and quadruples the nested loop's.

### Phase 5a: Rule-based optimizer

`src/optimizer.rs` rewrites every plan, including subquery plans, before execution. `\optimize` in the REPL turns it off for comparison.

- **Constant folding**: expressions that read no columns are evaluated once at plan time (`DATE '1998-12-01' - INTERVAL '90' DAY` becomes `DATE '1998-09-02'`), boolean logic is simplified (`x AND TRUE` becomes `x`), and always-true filters are removed. Expressions that would fail, such as `1 / 0`, are left alone so the error only happens if the row actually reaches them.
- **Filter pushdown**: each condition moves as far down as is legal. Single-table conditions land on their table's scan, and conditions spanning both sides of an inner join become join conditions. Conditions on GROUP BY keys move below the aggregate, and conditions pass through derived tables, sorts, and DISTINCT. Outer joins follow the precise rules: a WHERE condition can move into the preserved side, an ON condition can move into the optional side, and nothing moves across a FULL JOIN. Nothing moves below a LIMIT or a scalar aggregate.
- **Projection pruning**: each node keeps only the columns something above it reads. Scans copy only those columns out of storage (Q6 reads 4 of lineitem's 16), unused aggregates are dropped, and columns referenced only by correlated subqueries are kept.
- **Tests**: every TPC-H query, plus 1,200 randomized queries covering outer joins, derived tables, grouping, LIMIT, DISTINCT, and correlated subqueries, must give bit-identical results with and without the optimizer. Each of five classic optimizer bugs fails at least one test. Those bugs are: pushing past the NULL-padded side of an outer join, pushing a preserved-side ON condition, pushing through LIMIT, pushing below a scalar aggregate, and pruning a correlated column. The scalar-aggregate case initially went undetected, which led to a stronger test.

**Optimizer off vs on**, TPC-H scale 0.05 (300k lineitem rows), release build, best of 3:

| Query | Off | On | Speedup |
|---|---|---|---|
| Q1 | 371 ms | 324 ms | 1.1× |
| Q3 | 1,207 ms | 164 ms | 7.4× |
| Q6 | 173 ms | 70 ms | 2.5× |
| Q8 | 10,816 ms | 345 ms | 31× |
| Q13 | 305 ms | 189 ms | 1.6× |
| Q22 | 41,694 ms | 40,137 ms | 1.0× |

Q8 gains the most: filtering `part` before its cross product with `supplier` shrinks that product about 150-fold. Q22 is dominated by a correlated `NOT EXISTS` that re-runs once per customer, which no rule here changes; that's subquery decorrelation, in 5b.

### Phase 5b: Join ordering and subquery decorrelation

- **Statistics** (`src/catalog.rs`): row counts, plus per-column distinct and NULL counts, computed on first use and cached until the table is replaced.
- **Cardinality estimation** (`src/cardinality.rs`): estimates each plan node's output size from those statistics, using standard heuristics (`col = literal` keeps 1/distinct of the rows, an equi-join produces `|L|·|R| / max(distinct keys)`).
- **Join ordering** (`src/join_order.rs`): flattens each chain of inner joins and orders it greedily. It starts from the smallest input and repeatedly adds the connected input that keeps the intermediate result smallest, keeping the written order when that's estimated cheaper. Every hash join, outer joins included, builds its table on the smaller input; `a LEFT JOIN b` becomes `b RIGHT JOIN a` when b is bigger. Q8 no longer has a cross product: it starts from the single `AMERICA` region row and follows foreign keys outward.
- **Semi and anti joins** (`src/exec.rs`): a semi join keeps each left row that has a match, once; an anti join keeps each left row without one. Both are hash-based when there's an equality key.
- **Decorrelation** (`src/decorrelate.rs`): `EXISTS`, `NOT EXISTS`, and `IN` subqueries in WHERE and HAVING become semi and anti joins, with the subquery's correlated conditions as the join condition. They run once instead of once per outer row. The rewrite is skipped when it wouldn't be exactly equivalent: `NOT IN` (whose NULL behavior an anti join doesn't model), subqueries that aggregate or use LIMIT, and subqueries whose FROM clause reads the outer row.
- **Tests**: optimizer-on-vs-off comparisons now treat results without ORDER BY as multisets, since join reordering legitimately changes row order. Semi and anti joins are checked against `EXISTS`/`NOT EXISTS` on 200 random table pairs. Each decorrelation safety rule has a test that fails without it. Two findings came out of this work. A test caught that outer joins weren't getting the smaller build side (Q13 built its hash table on `orders` instead of `customer`). Mutation testing then showed one decorrelation restriction was unnecessary: a subquery nested inside a correlated condition can still see the outer row from within the join condition, so that case is now decorrelated too.

**Phase 5a vs 5b**, TPC-H scale 0.05, release build, best of 3:

| Query | 5a | 5b | Speedup |
|---|---|---|---|
| Q3 | 164 ms | 107 ms | 1.5× |
| Q8 | 345 ms | 132 ms | 2.6× |
| Q13 | 189 ms | 174 ms | 1.1× |
| Q22 | 40,137 ms | 76 ms | ~525× |

**The whole optimizer**, off vs on (same setup):

| Query | Off | On | Speedup |
|---|---|---|---|
| Q1 | 371 ms | 305 ms | 1.2× |
| Q3 | 1,207 ms | 107 ms | 11× |
| Q6 | 173 ms | 70 ms | 2.5× |
| Q8 | 10,816 ms | 132 ms | 82× |
| Q13 | 305 ms | 174 ms | 1.8× |
| Q22 | 41,694 ms | 76 ms | ~550× |

### Phase 6: Benchmarks against SQLite

- **CSV export** (`src/csv.rs`): the inverse of the loader, round-trip tested with quotes, delimiters, line breaks, NULLs, and empty strings.
- **`quarry-bench`** (`src/bin/bench.rs`): generates TPC-H data, times each query in Quarry, exports the tables to an in-memory SQLite database (through the `sqlite3` tool, so Quarry stays dependency-free), runs SQLite-dialect versions of the same queries in two index setups, and compares every answer. See [Results](#results).

### Phase 7a: Parquet reader

Reads Parquet files with `\load <table> <file.parquet>`, with no Parquet library (`src/parquet/`):

- **Thrift compact protocol decoder** (`thrift.rs`) for the file footer and page headers. It decodes into a generic tree keyed by field id, so it doesn't need code generated from Parquet's schema definition, and it skips fields it doesn't know.
- **Page decoding** (`encoding.rs`, `reader.rs`): PLAIN and dictionary encodings, the RLE / bit-packing hybrid used for NULL flags and dictionary indices, and version 1 and 2 data pages, including chunks that switch from dictionary to PLAIN partway through.
- **Snappy decompression** (`snappy.rs`), the default codec for pyarrow, DuckDB, and Spark, in about 100 lines.
- **Types**: booleans, integers, floats, strings, dates, and decimals (loaded as DOUBLE). Nested columns, timestamps, and other codecs (ZSTD, GZIP) are rejected with an error naming the column. A million-row file written by DuckDB loads in under half a second.

### Phase 7b: Postgres wire protocol

`quarry-server` speaks version 3 of the Postgres protocol, so standard clients connect to Quarry as if it were Postgres (`src/pgwire/`):

```
$ cargo run --release --bin quarry-server -- --tpch 0.01 --load items=data.parquet
$ psql -h localhost -p 5433
quarry=> select l_returnflag, count(*) from lineitem group by l_returnflag order by l_returnflag;
```

- **Startup and simple queries** (`protocol.rs`, `session.rs`): declines SSL, reports server parameters, and answers each statement with typed column descriptions, text-format rows, and errors with SQLSTATE codes (`42P01` undefined table, `42601` syntax error with the character position psql uses to draw its caret, and so on).
- **Extended query protocol** (`extended.rs`): Parse, Bind, Describe, Execute, Close, and Sync, with prepared statements, `$1` parameters in text or binary format (including Postgres's base-10000 binary NUMERIC), portals that hand out rows in batches, and discarding messages after an error until the next Sync. This is what psycopg and most drivers use for every query.
- **Transactions as clients expect them**: Quarry is read-only, but `BEGIN`, `COMMIT`, `ROLLBACK`, and `SET` are accepted and the transaction status is tracked, including the failed state after an error, because drivers check it.
- **One thread per connection** sharing one catalog. Making that possible meant replacing the catalog's statistics cache `RefCell` with a `Mutex`.
- **Tested** by a protocol-level client in `tests/pgwire.rs`, against real clients (psql 16 and psycopg 3, in `tests/clients/psycopg_check.py`), and by the DuckDB differential tests, which send every query through psycopg.

Listens on localhost only, since there is no authentication. psql's `\d` commands query Postgres's system catalogs, which Quarry doesn't have.

### Phase 8a: Scale factor 1, and DuckDB

- **`quarry-bench`** now also runs DuckDB (through the `duckdb` shell), with one thread and with all cores, and runs up to scale factor 1. The tables are exported to CSV once and loaded into each engine.
- **Matching DuckDB's arithmetic**: DuckDB evaluates `0.06 - 0.01` as an exact DECIMAL, while Quarry and SQLite use floating point (see [Known limitations](#known-limitations)), so Q6's bounds are cast to DOUBLE in DuckDB's copy of the query. Without that, the engines disagree about which 5% and 7% discounts fall inside the range.
- At scale factor 1 Quarry needs about 3.5 GB of memory: generating the data takes about 4 seconds and all six queries run in about 13.

### Phase 8b: Differential testing against DuckDB

- **A random query generator** (`tests/difftest/duckdb_diff.py`) builds type-correct SQL from the features both engines share: inner, outer, and cross joins, derived tables, `CASE`, `COALESCE`, string functions, `BETWEEN`, `IN` lists with NULLs, `LIKE`, correlated subqueries of every kind, aggregates with `DISTINCT`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, and `OFFSET`.
- **Each round** generates three small random tables, loads them into DuckDB and into a fresh `quarry-server`, and compares results as multisets (or in order, when the query has `ORDER BY` and `LIMIT`). Quarry is queried through psycopg, so the wire protocol is tested too.
- **What it found**: Quarry evaluated `x IN (subquery)` as NULL whenever `x` was NULL, but SQL says the answer is FALSE when the subquery returns no rows, since there is nothing to be unsure about. So `NULL NOT IN (empty)` was NULL instead of TRUE, and correlated `NOT IN` filters silently dropped rows. Before the fix about 1.5% of random queries disagreed; after it, none do. It also turned up an internal error in DuckDB itself (a column-binding assertion on a `FULL JOIN` with a correlated subquery), which the harness reports separately.
- **In CI**, a second job runs 6,000 random queries (three fixed seeds) and the psycopg client check on every push.

## Known limitations

- **DECIMAL is stored as DOUBLE.** TPC-H's prices and discounts are exact decimals. In binary floating point, `0.06 + 0.01` is `0.06999…`, so Q6's `l_discount BETWEEN 0.05 AND 0.07` drops every 7% discount that a real database would keep. SQLite stores these columns as doubles too, and the benchmark makes DuckDB's literals doubles, so the engines agree, but all would differ from exact decimal answers.
- **The optimizer's search is simple.** Join orders are left-deep and chosen greedily, not by exhaustive search, and estimates come from distinct counts, not histograms. Scalar subqueries and subqueries that aggregate still run once per outer row.
- **Execution is row-at-a-time and single-threaded**, with a tagged value per cell, which is why DuckDB is 17–60× faster.
- **Memory**: columns store an `Option` per value (16 bytes for a number, plus a heap allocation per string), so scale factor 1 takes about 3.5 GB, several times the raw data size.
- **No indexes**, and tables live in memory only, disappearing when the REPL exits.
- **Generated data, not official `dbgen` output**: sizes, keys, and value rules follow the TPC-H spec, but text fields are simpler, so results don't match the official answer set.

## Future work

- **Vectorized execution**: process columns in batches of ~1,000 values instead of one row at a time, the standard way to close the gap with DuckDB and SQLite's lead on Q3 and Q6.
- **Parallel execution**: split scans and hash joins across cores, as DuckDB does.
- **Exact DECIMAL type**, so results match the official TPC-H answers.
- **Indexes**, and saving tables to disk.
- **More TPC-H queries**, and loading official `dbgen` data.
