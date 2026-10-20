# Feature Studio test fixtures

All Feature Studio test inputs are **synthetic fixtures**, generated deterministically by
`tests/support/mod.rs` from closed-form price paths, then imported through the real RG-R01
Data Ground importer so they become registered, hash-verified datasets. Every fixture uses the
source ID prefix `synthetic-fixture:` and the broker `SYNTHETIC-FIXTURE`, and those labels are
carried into each feature set's lineage.

They are not historical market data, trading results, backtests, or acceptance benchmarks, and
they are not evidence of any edge. No file from MT5 `Common\Files` or any other external location
is read by these tests. Owner-supplied historical snapshots, when provided, must be imported
through Data Ground and are outside this fixture set.
