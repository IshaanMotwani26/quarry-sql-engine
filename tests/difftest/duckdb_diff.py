"""Differential testing: random queries must give the same answers in Quarry
and DuckDB.

Each round generates small random tables full of NULLs, duplicates, and
empty strings, loads them into DuckDB and into a fresh quarry-server, then
runs random queries in both and compares the results as multisets (or as
lists, for queries with ORDER BY and LIMIT). Quarry is queried through its
Postgres wire protocol with psycopg, so this also exercises the server.

    cargo build --release --bin quarry-server
    pip install duckdb "psycopg[binary]"
    python tests/difftest/duckdb_diff.py --rounds 20 --queries 100 --seed 1

Exits with status 1 and prints each mismatching query if any differ.
"""

import argparse
import datetime
import math
import os
import random
import socket
import subprocess
import sys
import tempfile
import time

import duckdb
import psycopg

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

TABLES = ["t0", "t1", "t2"]
# Column name -> type. Every table has the same columns, so any table can be
# joined to any other on any pair of same-typed columns.
COLUMNS = {"k": "int", "a": "int", "b": "float", "s": "str", "d": "date"}
SQL_TYPES = {"int": "BIGINT", "float": "DOUBLE", "str": "VARCHAR", "date": "DATE"}
STRINGS = ["a", "b", "ab", "ba", "abc", "B", ""]
BASE_DATE = datetime.date(2024, 1, 1)


# ---------------------------------------------------------------------------
# Data
# ---------------------------------------------------------------------------


def random_value(rng, ty, null_rate=0.2):
    if rng.random() < null_rate:
        return None
    if ty == "int":
        return rng.randint(-3, 5)
    if ty == "float":
        return rng.randint(-6, 6) / 2
    if ty == "str":
        return rng.choice(STRINGS)
    return BASE_DATE + datetime.timedelta(days=rng.randint(0, 5))


def random_table(rng):
    n = rng.randint(1, 12)
    rows = []
    for i in range(n):
        # The first row has no NULLs and a fractional float and a non-empty
        # string, so Quarry's CSV type inference sees every column's type.
        row = {c: random_value(rng, ty, 0 if i == 0 else 0.2) for c, ty in COLUMNS.items()}
        if i == 0:
            row["b"] = rng.choice([-1.5, 0.5, 2.5])
            row["s"] = rng.choice(STRINGS[:-1])
        rows.append(row)
    return rows


def write_csv(path, rows):
    def cell(v):
        if v is None:
            return ""  # unquoted empty field: NULL
        if isinstance(v, str):
            return '"' + v.replace('"', '""') + '"'  # quoted: always text
        if isinstance(v, float):
            return repr(v)  # always has a decimal point
        return str(v)

    with open(path, "w", newline="") as f:
        f.write(",".join(COLUMNS) + "\n")
        for row in rows:
            f.write(",".join(cell(row[c]) for c in COLUMNS) + "\n")


# ---------------------------------------------------------------------------
# Query generation
# ---------------------------------------------------------------------------


class Gen:
    """Generates type-correct SQL in the dialect both engines accept."""

    def __init__(self, rng):
        self.rng = rng
        self.alias_counter = 0

    def alias(self):
        self.alias_counter += 1
        return f"x{self.alias_counter}"

    def chance(self, p):
        return self.rng.random() < p

    def literal(self, ty):
        r = self.rng
        if ty == "int":
            return str(r.randint(-3, 5))
        if ty == "float":
            return f"CAST({r.randint(-6, 6) / 2} AS DOUBLE)"
        if ty == "str":
            return "'" + r.choice(STRINGS) + "'"
        if ty == "date":
            return f"DATE '{BASE_DATE + datetime.timedelta(days=r.randint(0, 5))}'"
        return r.choice(["TRUE", "FALSE"])

    def column(self, scope, ty):
        cols = [f"{a}.{c}" for a in scope for c, t in COLUMNS.items() if t == ty]
        return self.rng.choice(cols) if cols else self.literal(ty)

    def expr(self, scope, ty, depth=0):
        """An expression of type `ty` over the tables aliased in `scope`."""
        r = self.rng
        if ty == "bool":
            return self.predicate(scope, depth)
        if depth >= 3 or self.chance(0.45):
            return self.column(scope, ty) if self.chance(0.75) else self.literal(ty)
        d = depth + 1
        choice = r.random()
        if choice < 0.2:
            return (
                f"CASE WHEN {self.predicate(scope, d)} THEN {self.expr(scope, ty, d)} "
                f"ELSE {self.expr(scope, ty, d)} END"
            )
        if choice < 0.3:
            return f"coalesce({self.expr(scope, ty, d)}, {self.expr(scope, ty, d)})"
        if ty in ("int", "float"):
            if choice < 0.65:
                op = r.choice(["+", "-", "*"])
                other = r.choice(["int", ty])
                return f"({self.expr(scope, ty, d)} {op} {self.expr(scope, other, d)})"
            if choice < 0.72:
                return f"abs({self.expr(scope, ty, d)})"
            if choice < 0.75:
                if ty == "float":
                    return f"round({self.expr(scope, 'float', d)})"
                # Parenthesized, or `-` and a negative literal would make `--`.
                return f"(-({self.expr(scope, 'int', d)}))"
            if ty == "int" and choice < 0.85:
                return f"length({self.expr(scope, 'str', d)})"
            if ty == "int" and self.outer is not None and choice < 0.93:
                return self.scalar_subquery(scope, d)
            if ty == "float":
                return f"CAST({self.expr(scope, 'int', d)} AS DOUBLE)"
            return self.column(scope, ty)
        if ty == "str":
            if choice < 0.5:
                return f"({self.expr(scope, 'str', d)} || {self.expr(scope, 'str', d)})"
            if choice < 0.7:
                return f"{r.choice(['upper', 'lower'])}({self.expr(scope, 'str', d)})"
            if choice < 0.85:
                return (
                    f"substring({self.expr(scope, 'str', d)}, {r.randint(1, 3)}, {r.randint(0, 2)})"
                )
            if choice < 0.92:
                return f"CAST({self.expr(scope, r.choice(['int', 'date']), d)} AS VARCHAR)"
            return self.column(scope, "str")
        return self.column(scope, ty)

    def predicate(self, scope, depth=0):
        r = self.rng
        d = depth + 1
        if depth >= 3:
            return self.comparison(scope, d)
        choice = r.random()
        if choice < 0.3:
            return self.comparison(scope, d)
        if choice < 0.45:
            op = r.choice(["AND", "OR"])
            return f"({self.predicate(scope, d)} {op} {self.predicate(scope, d)})"
        if choice < 0.5:
            return f"NOT ({self.predicate(scope, d)})"
        if choice < 0.58:
            ty = r.choice(list(SQL_TYPES))
            return f"{self.expr(scope, ty, d)} IS {r.choice(['', 'NOT '])}NULL"
        if choice < 0.65:
            ty = r.choice(["int", "float", "date"])
            lo, hi = sorted([self.literal(ty), self.literal(ty)])
            neg = r.choice(["", "NOT "])
            return f"{self.expr(scope, ty, d)} {neg}BETWEEN {lo} AND {hi}"
        if choice < 0.73:
            ty = r.choice(["int", "str"])
            items = [self.literal(ty) for _ in range(r.randint(1, 3))]
            if self.chance(0.3):
                items.append("NULL")
            neg = r.choice(["", "NOT "])
            return f"{self.expr(scope, ty, d)} {neg}IN ({', '.join(items)})"
        if choice < 0.8:
            pattern = r.choice(["a%", "%b", "_", "%", "a_", "%a%", ""])
            neg = r.choice(["", "NOT "])
            return f"{self.expr(scope, 'str', d)} {neg}LIKE '{pattern}'"
        if choice < 0.9:
            return self.exists(scope, d)
        return self.in_subquery(scope, d)

    def comparison(self, scope, depth):
        ty = self.rng.choice(list(SQL_TYPES))
        op = self.rng.choice(["=", "<>", "<", "<=", ">", ">="])
        return f"{self.expr(scope, ty, depth)} {op} {self.expr(scope, ty, depth)}"

    # Subqueries, usually correlated with the enclosing query.

    def correlation(self, inner, scope):
        c = self.rng.choice(["k", "a", "s"])
        return f"{inner}.{c} = {self.rng.choice(scope)}.{c}"

    def exists(self, scope, depth):
        a = self.alias()
        conds = []
        if self.chance(0.8):
            conds.append(self.correlation(a, scope))
        if self.chance(0.5):
            conds.append(self.predicate([a], depth + 1))
        where = f" WHERE {' AND '.join(conds)}" if conds else ""
        neg = self.rng.choice(["", "NOT "])
        return f"{neg}EXISTS (SELECT * FROM {self.rng.choice(TABLES)} {a}{where})"

    def in_subquery(self, scope, depth):
        a = self.alias()
        ty = self.rng.choice(["int", "str"])
        c = self.rng.choice([c for c, t in COLUMNS.items() if t == ty])
        conds = []
        if self.chance(0.5):
            conds.append(self.correlation(a, scope))
        if self.chance(0.5):
            conds.append(self.predicate([a], depth + 1))
        where = f" WHERE {' AND '.join(conds)}" if conds else ""
        neg = self.rng.choice(["", "NOT "])
        return (
            f"{self.expr(scope, ty, depth)} {neg}IN "
            f"(SELECT {a}.{c} FROM {self.rng.choice(TABLES)} {a}{where})"
        )

    def scalar_subquery(self, scope, depth):
        a = self.alias()
        agg = self.rng.choice(["max", "min", "count", "sum"])
        where = f" WHERE {self.correlation(a, scope)}" if self.chance(0.7) else ""
        return f"(SELECT {agg}({a}.a) FROM {self.rng.choice(TABLES)} {a}{where})"

    # Whole queries.

    def table_ref(self):
        """A base table, or sometimes a derived table with the same columns."""
        r = self.rng
        if not self.chance(0.15):
            return r.choice(TABLES)
        a = self.alias()
        t = r.choice(TABLES)
        if self.chance(0.5):
            # Grouped: one row per k, with aggregated columns of the same types.
            cols = (
                f"{a}.k AS k, max({a}.a) AS a, sum({a}.b) AS b, min({a}.s) AS s, max({a}.d) AS d"
            )
            return f"(SELECT {cols} FROM {t} {a} GROUP BY {a}.k)"
        where = f" WHERE {self.predicate([a], 2)}" if self.chance(0.6) else ""
        distinct = "DISTINCT " if self.chance(0.3) else ""
        cols = ", ".join(f"{a}.{c} AS {c}" for c in COLUMNS)
        return f"(SELECT {distinct}{cols} FROM {t} {a}{where})"

    def from_clause(self):
        r = self.rng
        first = self.alias()
        scope = [first]
        sql = f"{self.table_ref()} {first}"
        for _ in range(r.choices([0, 1, 2], [3, 5, 2])[0]):
            a = self.alias()
            kind = r.choice(["JOIN", "JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN"])
            if kind == "CROSS JOIN":
                sql += f" CROSS JOIN {self.table_ref()} {a}"
            else:
                c = r.choice(list(COLUMNS))
                on = f"{r.choice(scope)}.{c} = {a}.{c}"
                if self.chance(0.3):
                    on += f" AND {self.predicate(scope + [a], 2)}"
                sql += f" {kind} {self.table_ref()} {a} ON {on}"
            scope.append(a)
        return sql, scope

    def query(self):
        r = self.rng
        self.alias_counter = 0
        self.outer = None
        from_sql, scope = self.from_clause()
        self.outer = scope
        where = f" WHERE {self.predicate(scope)}" if self.chance(0.7) else ""
        shape = r.random()
        if shape < 0.45:
            n = r.randint(1, 4)
            items = [
                f"{self.expr(scope, r.choice(list(SQL_TYPES) + ['bool']))} AS c{i}"
                for i in range(n)
            ]
            distinct = "DISTINCT " if self.chance(0.2) else ""
            sql = f"SELECT {distinct}{', '.join(items)} FROM {from_sql}{where}"
            if self.chance(0.25):
                order = ", ".join(f"c{i}" for i in range(n))
                sql += f" ORDER BY {order} LIMIT {r.randint(0, 5)}"
                if self.chance(0.3):
                    sql += f" OFFSET {r.randint(0, 3)}"
                return sql, True
            return sql, False
        # Aggregation, grouped or not.
        groups = []
        if shape < 0.85:
            groups = r.sample([f"{a}.{c}" for a in scope for c in COLUMNS], r.randint(1, 2))
        aggs = []
        for _ in range(r.randint(1, 3)):
            func = r.choice(["count", "sum", "min", "max", "avg", "count_star"])
            if func == "count_star":
                aggs.append("count(*)")
                continue
            ty = r.choice(["int", "float"] if func in ("sum", "avg") else list(SQL_TYPES))
            distinct = "DISTINCT " if self.chance(0.2) else ""
            aggs.append(f"{func}({distinct}{self.expr(scope, ty, 1)})")
        items = groups + aggs
        select = ", ".join(f"{e} AS c{i}" for i, e in enumerate(items))
        sql = f"SELECT {select} FROM {from_sql}{where}"
        if groups:
            sql += " GROUP BY " + ", ".join(groups)
            if self.chance(0.4):
                having = r.choice(
                    [
                        f"count(*) > {r.randint(0, 2)}",
                        f"sum({self.column(scope, 'int')}) > {r.randint(-2, 3)}",
                        f"min({self.column(scope, 'str')}) < '{r.choice(STRINGS)}'",
                        f"max({self.column(scope, 'float')}) IS NULL",
                        f"count(DISTINCT {self.column(scope, 'int')}) >= 1",
                    ]
                )
                sql += f" HAVING {having}"
        return sql, False


# ---------------------------------------------------------------------------
# Running and comparing
# ---------------------------------------------------------------------------


def normalize(v):
    """Makes values from the two drivers comparable."""
    if v is None or isinstance(v, (bool, str, datetime.date)):
        return (type(v).__name__ if v is not None else "null", v)
    x = float(v)  # ints, floats, and Decimals compare as numbers
    if x == 0:
        x = 0.0  # -0.0 and 0.0 are the same answer
    return ("num", float(f"{x:.9g}"))


def same_rows(q_rows, d_rows, ordered):
    q = [tuple(normalize(v) for v in row) for row in q_rows]
    d = [tuple(normalize(v) for v in row) for row in d_rows]
    if not ordered:
        key = lambda row: [(t, str(v)) for t, v in row]  # noqa: E731
        q, d = sorted(q, key=key), sorted(d, key=key)
    return q == d


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_server(server, tables_dir, port):
    args = [server, "--port", str(port)]
    for t in TABLES:
        args += ["--load", f"{t}={os.path.join(tables_dir, t + '.csv')}"]
    proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    for _ in range(100):
        try:
            return proc, psycopg.connect(
                f"host=127.0.0.1 port={port} user=difftest dbname=quarry", autocommit=True
            )
        except psycopg.OperationalError:
            if proc.poll() is not None:
                sys.exit(f"quarry-server exited: {proc.stderr.read().decode()}")
            time.sleep(0.05)
    sys.exit("quarry-server did not start")


def run(conn_fn, sql):
    try:
        return "ok", conn_fn(sql)
    except Exception as e:  # each engine's own error type
        return "error", str(e).strip().splitlines()[0]


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--rounds", type=int, default=10)
    p.add_argument("--queries", type=int, default=100, help="queries per round")
    p.add_argument("--seed", type=int, default=1)
    exe = "quarry-server.exe" if os.name == "nt" else "quarry-server"
    p.add_argument("--server", default=os.path.join(ROOT, "target", "release", exe))
    p.add_argument("--show", type=int, default=5, help="mismatches to print")
    args = p.parse_args()

    if not os.path.exists(args.server):
        sys.exit(f"{args.server} not found; run: cargo build --release --bin quarry-server")
    rng = random.Random(args.seed)
    gen = Gen(rng)
    total = mismatches = both_failed = 0
    duck_internal = []
    shown = 0
    with tempfile.TemporaryDirectory() as tmp:
        for round_no in range(args.rounds):
            data = {t: random_table(rng) for t in TABLES}
            for t, rows in data.items():
                write_csv(os.path.join(tmp, t + ".csv"), rows)
            duck = duckdb.connect()
            for t, rows in data.items():
                cols = ", ".join(f"{c} {SQL_TYPES[ty]}" for c, ty in COLUMNS.items())
                duck.execute(f"CREATE TABLE {t} ({cols})")
                duck.executemany(
                    f"INSERT INTO {t} VALUES ({', '.join('?' for _ in COLUMNS)})",
                    [[row[c] for c in COLUMNS] for row in rows],
                )
            proc, conn = start_server(args.server, tmp, free_port())
            try:
                for _ in range(args.queries):
                    sql, ordered = gen.query()
                    total += 1
                    q = run(lambda s: conn.execute(s).fetchall(), sql)
                    d = run(lambda s: duck.execute(s).fetchall(), sql)
                    if q[0] == "error" and d[0] == "error":
                        both_failed += 1
                        continue
                    if d[0] == "error" and d[1].startswith("INTERNAL Error"):
                        # A bug in DuckDB, not a disagreement about the answer.
                        duck_internal.append((sql, q))
                        continue
                    if q[0] == "ok" and d[0] == "ok" and same_rows(q[1], d[1], ordered):
                        continue
                    mismatches += 1
                    if shown < args.show:
                        shown += 1
                        print(f"--- mismatch (round {round_no}, seed {args.seed})\n{sql};")
                        print(f"  quarry: {q[1] if q[0] == 'error' else sorted(q[1], key=repr)}")
                        print(f"  duckdb: {d[1] if d[0] == 'error' else sorted(d[1], key=repr)}")
                        for t, rows in data.items():
                            print(f"  {t}: {[tuple(r.values()) for r in rows]}")
            finally:
                conn.close()
                proc.terminate()
                proc.wait()
                duck.close()

    for sql, q in duck_internal[: args.show]:
        print(f"--- DuckDB internal error (Quarry: {q[1] if q[0] == 'error' else q[1]})\n{sql};")
    print(
        f"{total} queries, {mismatches} mismatches, "
        f"{both_failed} rejected by both engines, "
        f"{len(duck_internal)} DuckDB internal errors (seed {args.seed})"
    )
    sys.exit(1 if mismatches else 0)


if __name__ == "__main__":
    main()
