# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

This is a Rust workspace containing two crates:
- `clickhouse-arrow`: High-performance ClickHouse client with native protocol and Arrow integration
- `clickhouse-arrow-derive`: Procedural macros for the `Row` derive

The project focuses on efficient data operations with ClickHouse while maintaining compatibility with the Arrow ecosystem.

## Common Development Commands

### Building
```bash
# Build all workspace members
cargo build

# Build with all features
cargo build --all-features

# Build release with optimizations
cargo build --release

# Build with LTO for maximum performance
cargo build --profile release-lto
```

### Testing
```bash
# Run all tests (requires test-utils feature for integration tests)
cargo test --features test-utils

# Run specific integration test suites
cargo test --test e2e_arrow --features test-utils
cargo test --test e2e_native --features "test-utils,derive"

# Run with output visible
cargo test --features test-utils -- --nocapture

# Test with specific ClickHouse version
CLICKHOUSE_VERSION="25.6" cargo test --all-features
```

### Linting and Formatting
```bash
# Format code
cargo fmt

# Run clippy with all features
cargo clippy --all-features --all-targets

# Fix clippy warnings
cargo clippy --fix
```

### Running Examples
All examples require the `test-utils` feature:
```bash
cargo run --example select --features test-utils
cargo run --example insert --features test-utils
cargo run --example pool --features test-utils

# Control number of runs
EXAMPLE_RUNS=50 cargo run --example insert --features test-utils
```

### Benchmarking
```bash
cargo bench --features test-utils
cargo bench --bench insert --features test-utils
cargo bench --bench query --features test-utils
```

### Protocol Debugging
```bash
# Analyze raw TCP binary data for ClickHouse protocol debugging
python3 chc-tcp.py "SELECT if(number % 2 = 0, 'yes', number) as v FROM system.numbers LIMIT 3"

# Use this tool to inspect binary wire format when implementing complex types like Variant
```

## Architecture

### Core Components

1. **Client Module** (`src/client/`)
   - `builder.rs`: ClientBuilder for connection configuration
   - `mod.rs`: Main client implementation with query/insert methods
   - Connection pooling via bb8 (optional feature)

2. **Protocol Implementation** (`src/native/`)
   - Native ClickHouse wire protocol
   - Packet encoding/decoding
   - Compression support (LZ4, ZSTD)

3. **Data Formats**
   - **Arrow Format** (`src/arrow/`): Arrow RecordBatch integration
   - **Native Format** (`src/formats/`): Internal type system

4. **Type System** (`src/types/`)
   - Comprehensive ClickHouse type support
   - Arrow type mapping and conversion
   - Special handling for LowCardinality, Nullable, Arrays

### Key Design Patterns

1. **Format Abstraction**: `ArrowFormat` and `NativeFormat` traits allow switching between Arrow and native representations
2. **Streaming**: All queries return async streams for memory-efficient data processing
3. **Zero-Copy**: Optimized for minimal allocations during data transfer
4. **Builder Pattern**: Consistent API for client configuration

### Important Implementation Details

1. **Arrow Round-Trip Considerations**:
   - `Utf8` types default to `Binary` for performance (configurable via `strings_as_strings`)
   - `Nullable(Array)` is converted to `Array(Nullable)` by default
   - `Dictionary` types map to `LowCardinality`

2. **Compression**: Automatically negotiated with server, supports LZ4 and ZSTD

3. **Connection Pooling**: Optional bb8-based pooling with the `pool` feature

4. **Feature Flags**:
   - `derive`: Enable Row derive macro
   - `serde`: JSON serialization support
   - `pool`: Connection pooling
   - `inner_pool`: Spawns multiple "inner connections" for improved concurrency
   - `cloud`: ClickHouse Cloud support
   - `test-utils`: Required for tests/examples/benchmarks

## Testing Strategy

- Unit tests are inline with modules
- Integration tests require a ClickHouse container (handled automatically by testcontainers)
- E2E tests cover Arrow format, native format, compatibility, and row binary
- Benchmarks measure insert/query performance and compression

## Development Notes

- The project uses Rust 2024 edition
- Extensive clippy lints are configured in the workspace Cargo.toml
- Custom disallowed methods are defined in clippy.toml
- Test containers are automatically managed, set `DISABLE_CLEANUP=true` to keep containers running

## Native Protocol Documentation

The `native_protocol/` folder contains generated documentation about ClickHouse's native wire protocol implementation:

- **01-core-protocol.md**: Core protocol concepts, handshake, and basic packet structure
- **02-packet-types.md**: Detailed packet types (Hello, Data, Query, etc.) and their formats
- **03-data-serialization.md**: Data type serialization/deserialization, including complex types like Arrays, Maps, Tuples, and Variants
- **04-state-management.md**: Connection state, query state, and error handling
- **05-advanced-features.md**: Compression, query parameters, progress reporting, and profiling
- **TODO.md**: Implementation status and pending tasks
- **CLAUDE.md**: Additional protocol implementation notes

These documents provide detailed insights into the binary wire format and can be referenced when implementing or debugging protocol features, especially for complex types like Variant.

## Variant Type Implementation

### Overview
The Variant type in ClickHouse is a discriminated union that can hold one of several possible types. The implementation in this codebase handles the multi-stream architecture used by ClickHouse's native protocol.

### Key Implementation Details

1. **Discriminator Mapping**:
   - Types within a Variant are sorted alphabetically to determine discriminator values
   - Discriminator 0xFF (255) is reserved for NULL values
   - Example: `Variant(String, UInt64)` → String=0, UInt64=1 (alphabetical order)

2. **Wire Format**:
   - 8-byte version prefix (must be 0) - read during deserialize_prefix phase
   - Discriminators as byte array (one byte per row)
   - Column data for each type serialized separately (multi-stream architecture)
   - Data is grouped by discriminator type, not interleaved

3. **Deserialization Process**:
   - Read version prefix (8 bytes) during prefix phase
   - Read all discriminators
   - Count rows per discriminator type
   - Read column data for each type in discriminator order
   - Reconstruct values in original row order using offsets

4. **Current Status**:
   - Deserialization: ✅ Implemented and tested
   - Serialization: ✅ Implemented with comprehensive tests
   - COMPACT mode: ❌ TODO (BASIC mode implemented)
   - Nested Variants: ⚠️ Partially working (prefix handling implemented)

### Reference Implementation
The `ctx/clickhouse-go/` directory contains the ClickHouse Go driver source code which has a working Variant implementation. Key files:
- `ctx/clickhouse-go/lib/column/variant.go` - Main Variant column implementation
- `ctx/clickhouse-go/lib/chcol/variant.go` - Variant value type
- `ctx/clickhouse-go/tests/variant_test.go` - Test examples

Use this as a reference when implementing features or debugging issues with the Variant type.

### Technical Specifications
The `dynamic-containers-technical-spec.md` file contains the official ClickHouse technical specification for Dynamic and Variant types, including:
- Detailed wire format descriptions
- Serialization/deserialization algorithms
- Type registry and discriminator mapping rules
- SharedVariant handling for Dynamic type overflow
- Examples and edge cases

This specification should be consulted when implementing or debugging Dynamic and Variant type features.
