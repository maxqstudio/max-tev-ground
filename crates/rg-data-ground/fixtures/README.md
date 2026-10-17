# Data Ground parser fixtures

`eurusd-gbpusd-h1-h4.csv` contains five fixed OHLCV rows used only to exercise
CSV/Parquet parsing, UTC timestamps, multiple instruments/timeframes, and gap
classification. It is synthetic test fixture data and is not historical market
evidence, a trading result, or an acceptance benchmark.

The Parquet parser test writes an equivalent Parquet file into a temporary test
directory from these same fixed rows, then invokes the production Parquet reader.
