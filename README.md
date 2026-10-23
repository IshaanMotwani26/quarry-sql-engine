# Quarry

A SQL query engine written from scratch in Rust, with no dependencies. It parses, binds, optimizes, and executes analytical SQL, and runs six TPC-H queries on generated data with the same answers as SQLite.

```
SQL text → lexer → parser → binder → logical planner → optimizer → executor → results
```

The optimizer pushes filters down, prunes unused columns, folds constants, turns `EXISTS`/`IN` subqueries into semi and anti joins, and orders joins by estimated cost from table statistics. The executor runs hash joins, hash aggregation, sorting, and correlated subqueries over columnar in-memory tables.

## Results

`quarry-bench` runs TPC-H queries 1, 3, 6, 8, 13, and 22 in Quarry and in SQLite 3.45 (in memory, with `ANALYZE`), and compares every row of every answer.

**Correctness:** Quarry and SQLite return identical results for all six queries at scales 0.01, 0.05, and 0.1, in both SQLite setups below.

**Speed** at scale 0.1 (600k lineitem rows). Both engines are single-threaded; times are best of 5 on a Linux cloud VM and will vary by machine.

| Query | Quarry | SQLite, primary keys only | | SQLite, + foreign-key indexes | |
|---|---|---|---|---|---|
| Q1 | 353 ms | 478 ms | Quarry 1.4× faster | 524 ms | Quarry 1.5× faster |
| Q3 | 156 ms | 48 ms | SQLite 3.3× faster | 68 ms | SQLite 2.3× faster |
| Q6 | 119 ms | 67 ms | SQLite 1.8× faster | 69 ms | SQLite 1.7× faster |
| Q8 | 175 ms | 1,122 ms | Quarry 6.4× faster | 796 ms | Quarry 4.5× faster |
| Q13 | 204 ms | 357 ms | Quarry 1.8× faster | 136 ms | SQLite 1.5× faster |
| Q22 | 57 ms | 4,910 ms | Quarry 86× faster | 7 ms | SQLite 8.2× faster |

What the numbers show:

- **Against SQLite with only primary keys, Quarry is faster on 4 of 6 queries.** Its optimizer finds plans SQLite doesn't: it decorrelates Q22's `NOT EXISTS` into an anti join (86×), and orders Q8's eight-table join from statistics (6.4×).
- **Once SQLite also indexes the foreign keys, SQLite is faster on 4 of 6.** Quarry has no indexes, so it builds a hash table for every join, while SQLite looks rows up in a B-tree; Q22 in SQLite drops from 4.9 s to 7 ms. Quarry keeps Q8 here only because, at this scale, SQLite's planner chooses a poor plan when `nation(n_regionkey)` is indexed: without that one index, SQLite runs Q8 in 19 ms. (At scale 0.02 it picks the good plan either way.)
- **Quarry is consistently faster on Q1**, a full-table scan with eight aggregates, where indexes don't help. Quarry's columnar scan copies only the 7 columns the query reads.
- **SQLite is consistently faster on Q3 and Q6**: its compiled bytecode outruns Quarry's row-at-a-time interpreter, which copies a tagged value for every column of every row. Vectorized execution, listed under future work, is the standard fix.

Reproduce with `cargo run --release --bin quarry-bench -- 0.01 0.05 0.1 --runs 5` (needs `sqlite3` on your PATH for the comparison).

### How correctness is checked

201 tests, run in CI with `clippy -D warnings` and `rustfmt`:

- **Property tests**: random syntax trees must print and reparse identically.
- **Differential tests**: every query must return the same result with the optimizer on and off, and with hash and nested-loop joins (1,200 random queries plus all six TPC-H queries); semi and anti joins are checked against `EXISTS` on random tables.
- **Reference implementations**: TPC-H Q1, Q3, and Q6 are recomputed directly in Rust and compared.
- **Mutation testing**: deliberately planted bugs (wrong outer-join pushdown, NULL keys that match, a dropped join residual, and others) must each fail at least one test. This found two weak tests, which were strengthened, and one unnecessary optimizer restriction, which was removed.
- **SQLite**: the benchmark fails if any answer differs.

## Quick start

```bash
cargo test                               # 201 tests
cargo run --release                      # REPL; \? lists commands, \demo loads sample tables
cargo run --release --bin quarry-bench   # TPC-H benchmark against SQLite
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

## Known limitations

- **DECIMAL is stored as DOUBLE.** TPC-H's prices and discounts are exact decimals. In binary floating point, `0.06 + 0.01` is `0.06999…`, so Q6's `l_discount BETWEEN 0.05 AND 0.07` drops every 7% discount that a real database would keep. SQLite stores these columns as doubles too, so the two engines agree, but both would differ from the official TPC-H answers.
- **The optimizer's search is simple.** Join orders are left-deep and chosen greedily, not by exhaustive search, and estimates come from distinct counts, not histograms. Scalar subqueries and subqueries that aggregate still run once per outer row.
- **Execution is row-at-a-time**, with a tagged value per cell; see Q3 and Q6 in the results.
- **No indexes**, and tables live in memory only, disappearing when the REPL exits.
- **Generated data, not official `dbgen` output**: sizes, keys, and value rules follow the TPC-H spec, but text fields are simpler, so results don't match the official answer set.

## Future work

- **Vectorized execution**: process columns in batches of ~1,000 values instead of one row at a time, the standard way to close SQLite's lead on Q3 and Q6.
- **Exact DECIMAL type**, so results match the official TPC-H answers.
- **Indexes and on-disk storage**, for example a Parquet reader.
- **Postgres wire protocol**, so `psql` and Postgres drivers can connect.
- **More TPC-H queries**, and loading official `dbgen` data.
