# ClickHouse Native Protocol Integration Tests

This directory contains comprehensive integration tests that compare the output of our `clickhouse-test-client` against the official ClickHouse client to ensure compatibility and correctness.

## Structure

- `test_cases/` - Individual test case definitions organized by category
- `runner.py` - Python test runner that executes both clients and compares outputs
- `run_tests.sh` - Shell script wrapper for easy execution
- `results/` - Test results and diff outputs (generated)

## Running Tests

```bash
# Run all tests
./run_tests.sh

# Run specific category
./run_tests.sh basic_types

# Run with verbose output
./run_tests.sh --verbose

# Run and save detailed diffs
./run_tests.sh --save-diffs
```

## Test Categories

1. **Basic Types** - Fundamental ClickHouse data types
2. **Complex Types** - Arrays, tuples, maps, nested structures
3. **New Types** - Recently implemented types (Bool, Variant, Dynamic, etc.)
4. **Edge Cases** - NULL handling, empty data, boundary conditions
5. **Compatibility** - Cross-version compatibility scenarios

## Requirements

- ClickHouse server running on localhost:9000
- Python 3.7+ with `json`, `subprocess`, `difflib` modules
- Both `clickhouse client` and `clickhouse-test-client` in PATH