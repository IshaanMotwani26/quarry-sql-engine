"""Regenerates the Parquet test fixtures in this directory.

Requires pyarrow and duckdb (pip install pyarrow duckdb). The fixtures are
checked in, so this only needs to run if they change.
"""

import datetime
import decimal
import os

import pyarrow as pa
import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))

# Every type Quarry reads, with NULLs, edge values, and non-ASCII text.
# tests/parquet.rs checks the loaded table against this exact data.
TYPES = pa.table(
    {
        "id": pa.array([1, 2, 3, 4, 5, 6], pa.int64()),
        "qty": pa.array([10, None, -3, 2147483647, 0, None], pa.int32()),
        "flag": pa.array([True, False, None, True, True, False]),
        "price": pa.array([1.5, -0.25, None, 1e10, 0.1, 3.0], pa.float64()),
        "ratio": pa.array([0.5, 1.25, -2.0, None, 0.0, 8.0], pa.float32()),
        "name": pa.array(["apple", "", None, "naïve café", "apple", "a,b|c"]),
        "day": pa.array(
            [
                datetime.date(2024, 2, 29),
                datetime.date(1970, 1, 1),
                datetime.date(1969, 12, 31),
                None,
                datetime.date(1998, 12, 1),
                datetime.date(2000, 1, 1),
            ]
        ),
        "amount": pa.array(
            [decimal.Decimal(s) if s else None
             for s in ["12.34", "-0.05", None, "9999999999.99", "0.00", "1.10"]],
            pa.decimal128(12, 2),
        ),
        "tiny": pa.array([1, -128, 127, None, 0, 5], pa.int8()),
    },
    schema=pa.schema(
        [pa.field("id", pa.int64(), nullable=False)]
        + [pa.field(n, t) for n, t in [
            ("qty", pa.int32()), ("flag", pa.bool_()), ("price", pa.float64()),
            ("ratio", pa.float32()), ("name", pa.string()), ("day", pa.date32()),
            ("amount", pa.decimal128(12, 2)), ("tiny", pa.int8()),
        ]]
    ),
)


def pages_table(n=1200):
    """Enough rows for several row groups and many pages per column."""
    return pa.table(
        {
            "k": pa.array(range(n), pa.int64()),
            "g": pa.array([i % 7 for i in range(n)], pa.int32()),
            "s": pa.array([None if i % 11 == 0 else f"s{i % 13}" for i in range(n)]),
            "b": pa.array([i % 3 == 0 for i in range(n)]),
        }
    )


def write(name, table, **kwargs):
    # Leave out pyarrow's schema copy and min/max statistics to keep files small.
    pq.write_table(table, os.path.join(HERE, name), store_schema=False,
                   write_statistics=False, **kwargs)


def main():
    plain = dict(compression="none", use_dictionary=False, data_page_version="1.0")
    write("plain.parquet", TYPES, **plain)
    write("dictionary.parquet", TYPES, compression="none", use_dictionary=True)
    write("page_v2.parquet", TYPES, compression="none", data_page_version="2.0")
    write("pages.parquet", pages_table(), compression="none",
          row_group_size=500, data_page_size=256,
          dictionary_pagesize_limit=512,  # k falls back to PLAIN mid-chunk
          write_batch_size=50)  # page limits are checked once per batch

    nested = pa.table({"id": [1], "point": [{"x": 1, "y": 2}]})
    write("nested.parquet", nested, compression="none")
    timestamps = pa.table({"t": pa.array([0], pa.timestamp("us"))})
    write("timestamp.parquet", timestamps, compression="none")


if __name__ == "__main__":
    main()
