# Integration Test Usage Guide

## Quick Start

```bash
# Run all tests
./run_tests.sh

# Run specific categories
./run_tests.sh basic_types new_types

# Run with detailed output and save diffs
./run_tests.sh --verbose --save-diffs
```

## Test Results

The integration test runner compares JSON output between:
- **ClickHouse Client**: Official client using `JSONEachRow` format
- **Test Client**: Our native protocol client using `pretty` JSON format

Both outputs are normalized to JSON objects before comparison, so formatting differences are ignored.

## Example Test Run Output

```
🚀 Running integration tests...
📍 Test categories: ['basic_types']

📂 Running category: basic_types
   ✅ simple_integers
   ✅ simple_strings  
   ❌ complex_variant
   💥 large_data

📊 Test Summary:
   Total: 24
   ✅ Passed: 20
   ❌ Failed: 3
   💥 Errors: 1
```

## Status Indicators

- ✅ **PASS**: Both clients produced identical JSON output
- ❌ **FAIL**: Clients produced different JSON (functional difference)
- 💥 **ERROR**: One or both clients failed to execute the query

## Debugging Failed Tests

When tests fail, check the `results/` directory for:
- `test_results.json`: Complete test results with details
- `*_json_diff.diff`: Unified diffs showing JSON differences
- `*_parse_mismatch.diff`: Raw output when JSON parsing fails

## Adding New Tests

1. Create a YAML file in the appropriate `test_cases/` subdirectory
2. Follow the existing format:
   ```yaml
   name: "Test Category Name"
   description: "Description of what this tests"
   tests:
     - name: "test_case_name"
       query: "SELECT 'your ClickHouse query here'"
   ```

## Common Issues

1. **Binary data tests**: Some queries with binary data may fail due to UTF-8 encoding issues
2. **Floating point precision**: Minor differences in float representation
3. **Timestamp formatting**: Different timezone handling between clients
4. **Large data**: Memory or timeout issues with very large result sets

## Requirements

- ClickHouse server running on localhost:9000
- Python 3.7+ with PyYAML
- Built test client binary