# ClickHouse Arrow Test Client

A comprehensive test client for the `clickhouse-arrow` native format implementation. This client demonstrates how to use the library's native protocol support and provides utilities for testing various ClickHouse data types.

## Features

- **Native Protocol Support**: Uses ClickHouse's native TCP protocol via `clickhouse-arrow`
- **JSON I/O**: Input and output data in JSON format for easy integration
- **Type Testing**: Built-in commands to test all supported ClickHouse data types
- **Batch Operations**: Efficient batch insert capabilities
- **Flexible Configuration**: Support for compression, authentication, and connection options

## Installation

```bash
cd test-client
cargo build --release
```

## Usage

### Basic Connection

```bash
# Connect to local ClickHouse instance
./target/release/clickhouse-test-client --host localhost --port 9000 --user default --info

# Connect with authentication
./target/release/clickhouse-test-client --host example.com --port 9000 --user myuser --password mypass --info

# Connect with TLS
./target/release/clickhouse-test-client --host secure.example.com --port 9440 --secure --info
```

### Query Operations

```bash
# Simple query
./target/release/clickhouse-test-client --query "SELECT 1"

# Query with parameters (JSON format)
./target/release/clickhouse-test-client --query "SELECT {param:UInt64}" --params '{"param": 42}'

# Query with settings
./target/release/clickhouse-test-client --query "SELECT * FROM system.numbers LIMIT 5" --settings '{"max_rows_to_read": 10}'

# Pretty output format
./target/release/clickhouse-test-client --format pretty --query "SELECT name, type FROM system.columns LIMIT 3"
```

### Insert Operations

```bash
# Insert from JSON lines
echo '{"id": 1, "name": "Alice", "age": 30}' | ./target/release/clickhouse-test-client --insert test_table
echo '{"id": 2, "name": "Bob", "age": 25}' | ./target/release/clickhouse-test-client --insert test_table

# Insert from file
cat data.jsonl | ./target/release/clickhouse-test-client --insert my_table

# Insert with database specification
echo '{"col1": "value1", "col2": 123}' | ./target/release/clickhouse-test-client --insert mydb.mytable
```

### Type Testing

Test all supported ClickHouse native types:

```bash
# Test all implemented native types
./target/release/clickhouse-test-client --test-types

# Test with pretty output
./target/release/clickhouse-test-client --format pretty --test-types
```

### Server Information

```bash
# Get server version and uptime
./target/release/clickhouse-test-client --info

# Pretty format
./target/release/clickhouse-test-client --format pretty --info
```

## Configuration Options

### Connection Options

- `--host`: ClickHouse server hostname (default: localhost)
- `--port`: ClickHouse server port (default: 9000)
- `--user`: Database user (default: default)
- `--password`: Database password (default: empty)
- `--database`: Database name (default: default)
- `--secure`: Enable TLS/SSL connection
- `--compression`: Compression codec - `none`, `lz4`, `zstd` (default: lz4)

### Output Options

- `--format`: Output format - `json` or `pretty` (default: json)
- `--debug`: Enable debug logging

## Examples

### Testing New Types

The client includes comprehensive tests for all the newly implemented ClickHouse native types:

```bash
# Test Bool type
./target/release/clickhouse-test-client --query "SELECT true::Bool as native_bool, false::Bool as native_false"

# Test Nothing type  
./target/release/clickhouse-test-client --query "SELECT NULL::Nothing as nothing_val"

# Test Variant type (if supported by your ClickHouse version)
./target/release/clickhouse-test-client --query "SELECT 'hello'::Variant(String, UInt64) as variant_val"

# Test nested structures
./target/release/clickhouse-test-client --query "SELECT [1, 2, [3, 4]] as nested_array"
```

### Batch Processing

```bash
# Create test data
for i in {1..1000}; do
  echo "{\"id\": $i, \"value\": \"item_$i\", \"timestamp\": \"$(date -Iseconds)\"}"
done > test_data.jsonl

# Batch insert
cat test_data.jsonl | ./target/release/clickhouse-test-client --insert test.batch_data

# Verify
./target/release/clickhouse-test-client --query "SELECT count() FROM test.batch_data"
```

### Performance Testing

```bash
# Generate large dataset
seq 1 100000 | awk '{print "{\"id\": " $1 ", \"value\": " ($1 * $1) "}"}' > large_data.jsonl

# Time the insert
time cat large_data.jsonl | ./target/release/clickhouse-test-client --insert perf_test

# Query performance
time ./target/release/clickhouse-test-client --query "SELECT avg(value), count() FROM perf_test"
```

## JSON Input/Output Format

### Query Output

Each query result row is output as a JSON object:

```json
{"type": "data", "data": {"column_0": 42, "column_1": "hello"}}
```

### Error Format

Errors are returned in structured JSON:

```json
{"type": "error", "error": "Connection failed: timeout"}
```

### Insert Input

Insert data should be provided as JSON Lines format (one JSON object per line):

```jsonl
{"id": 1, "name": "Alice", "score": 95.5}
{"id": 2, "name": "Bob", "score": 87.2}
{"id": 3, "name": "Charlie", "score": 92.1}
```

## Type Support

This test client supports all ClickHouse native types implemented in the `clickhouse-arrow` library:

### Basic Types
- Integer types: `Int8`, `Int16`, `Int32`, `Int64`, `Int128`, `Int256`
- Unsigned integers: `UInt8`, `UInt16`, `UInt32`, `UInt64`, `UInt128`, `UInt256`
- Floating point: `Float32`, `Float64`
- Boolean: `Bool`
- String types: `String`, `FixedString(N)`

### Date/Time Types
- `Date`, `Date32`
- `DateTime`, `DateTime64`

### Complex Types
- `Array(T)`
- `Tuple(T1, T2, ...)`
- `Map(K, V)`
- `Nullable(T)`
- `LowCardinality(T)`

### Special Types
- `UUID`
- `IPv4`, `IPv6`
- `Enum8`, `Enum16`
- `Decimal32`, `Decimal64`, `Decimal128`, `Decimal256`

### Advanced Types (Newly Implemented)
- `AggregateFunction`: Aggregate function states
- `SimpleAggregateFunction`: Simple aggregate functions
- `Nothing`: Special null type
- `Nested`: Legacy nested structures
- `Variant`: Union types with discriminator
- `Dynamic`: Schema evolution support

## Development

To modify or extend the test client:

```bash
# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug ./target/release/clickhouse-test-client --debug --query "SELECT 1"

# Build for development
cargo build
```

## Error Handling

The client provides detailed error messages for common issues:

- Connection failures
- Authentication errors
- Query syntax errors
- Type conversion errors
- JSON parsing errors

All errors are returned in structured JSON format for easy parsing by automated tools.

## Integration with CI/CD

The JSON output format makes this client suitable for automated testing:

```bash
#!/bin/bash
# Test script example

# Test connection
if ! ./clickhouse-test-client --info > /dev/null 2>&1; then
  echo "ERROR: Cannot connect to ClickHouse"
  exit 1
fi

# Test basic types
if ! ./clickhouse-test-client --test-types > test_results.json; then
  echo "ERROR: Type tests failed"
  exit 1
fi

echo "All tests passed!"
```

This test client serves as both a practical tool for working with ClickHouse and a comprehensive example of how to use the `clickhouse-arrow` library's native format capabilities.