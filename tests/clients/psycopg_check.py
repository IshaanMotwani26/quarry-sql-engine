"""Checks quarry-server against a real Postgres driver.

Start the server, then run this script:

    cargo run --release --bin quarry-server -- --tpch 0.01
    pip install "psycopg[binary]"
    python tests/clients/psycopg_check.py

psycopg uses the extended query protocol, sends numbers, dates, and
decimals as binary parameters, wraps statements in BEGIN/COMMIT, and switches
to named prepared statements after a query runs five times.
"""

import datetime
import decimal
import sys

import psycopg

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5433


def main():
    with psycopg.connect(f"host=localhost port={PORT} user=quarry dbname=quarry") as conn:
        cur = conn.execute(
            "select n_name from nation where n_regionkey = %s order by n_name", (1,)
        )
        assert [r[0] for r in cur] == ["ARGENTINA", "BRAZIL", "CANADA", "PERU", "UNITED STATES"]

        row = conn.execute(
            "select %s as a, %s as b, %s as c, %s as d, %s as e",
            (None, "it's", 2.5, True, datetime.date(2024, 2, 29)),
        ).fetchone()
        assert row == (None, "it's", 2.5, True, datetime.date(2024, 2, 29)), row

        (q6,) = conn.execute(
            "select sum(l_extendedprice * l_discount) from lineitem "
            "where l_shipdate >= %s and l_shipdate < %s "
            "and l_discount between %s and %s and l_quantity < %s",
            (datetime.date(1994, 1, 1), datetime.date(1995, 1, 1),
             decimal.Decimal("0.05"), decimal.Decimal("0.07"), 24),
        ).fetchone()
        assert q6 > 0, q6

        try:
            conn.execute("select * from no_such_table")
            raise AssertionError("expected an error")
        except psycopg.errors.UndefinedTable:
            conn.rollback()

        for i in range(8):  # past psycopg's prepare threshold of 5
            (n,) = conn.execute(
                "select count(*) from orders where o_custkey = %s", (i + 1,)
            ).fetchone()
            assert n >= 0

        cur = conn.execute("select o_orderkey from orders order by o_orderkey limit 5")
        assert len(cur.fetchmany(2)) == 2 and len(cur.fetchall()) == 3

    print(f"psycopg {psycopg.__version__}: all checks passed (TPC-H Q6 revenue {q6:.2f})")


if __name__ == "__main__":
    main()
