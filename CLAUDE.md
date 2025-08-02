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

There's a Python UV script available for debugging ClickHouse native protocol over TCP:

```bash
# Analyze raw TCP binary data for ClickHouse protocol debugging
./scripts/chc-tcp.py "SELECT if(number % 2 = 0, 'yes', number) as v FROM system.numbers LIMIT 3"

# or write a pcap file
./scripts/chc-tcp.py "SELECT if(number % 2 = 0, 'yes', number) as v FROM system.numbers LIMIT 3" --pcap capture.pcap

# spawn a test container for specific clickhouse version then run query
./scripts/chc-tcp.py "SELECT * FROM system.numbers LIMIT 1000" --clickhouse-container-version 25.1
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

- **Unit Tests**: Core logic and individual components (inline with modules)
- **Integration Tests**: End-to-end operations with real services (require ClickHouse container, handled automatically by testcontainers)
- **E2E Tests**: Cover Arrow format, native format, compatibility, and row binary
- **Keep tests focused**: Test behavior, not implementation details
- **Benchmarks**: Measure insert/query performance and compression
- **Avoid test coverage boasting**: Tests exist for quality, not metrics

## Development Notes

- The project uses Rust 2024 edition
- Extensive clippy lints are configured in the workspace Cargo.toml
- Custom disallowed methods are defined in clippy.toml
- Test containers are automatically managed, set `DISABLE_CLEANUP=true` to keep containers running

## Communication Guidelines

When working on projects, use measured, specific language:

**Avoid overly confident terms**:
- Don't use: "comprehensive", "production ready", "robust", "enterprise-grade", "bulletproof", "seamless", "cutting-edge"
- Instead use: specific descriptions of what the code does

**Use factual, measured language**:
- "implements X pattern" rather than "elegantly implements"
- "handles Y scenario" rather than "comprehensively handles" 
- "supports Z formats" rather than "full support for Z formats"
- "processes N objects" rather than "efficiently processes large volumes"

**Focus on specifics**:
- Include actual numbers, concrete capabilities, and factual descriptions
- Describe what exists rather than aspirational qualities
- Use precise technical terms rather than marketing language

**Documentation style**:
- No emojis in README files or documentation
- Use plain text checkmarks and formatting instead of emoji indicators
- Remember: "I'm not trying to prove to the world that I'm competent"
- Avoid sections that show off (architecture diagrams, test coverage lists, etc.)
- Focus on what users need, not demonstrating technical prowess

## Git Commit Guidelines

**Commit Message Format**: Use standard, concise commit messages without any tool attribution:

```bash
git commit -m "feat: add pattern matching for selective deletion"
git commit -m "fix: resolve compatibility issue"
git commit -m "test: add unit tests"
```

**Important**: Do NOT include Claude Code attribution, co-author tags, or any generated-by notices in commit messages. Use clean, professional commit messages that focus on the actual changes made.

## Development Process

### Incremental Development Rules

1. **Small Changes Only**: Each change should be under 50 lines. If bigger, break it down.
2. **Test Every Change**: Run the full validation cycle after every code modification.
3. **No Exceptions**: Every warning must be fixed. No "I'll fix it later."

### Required Validation Cycle

Run this after every code change, in order:

```bash
cargo fmt          # Format code
cargo clippy       # Lint and catch issues
cargo test         # Run all tests
```

All must pass with zero warnings/errors before proceeding.

### Git Workflow

```bash
# After validation cycle passes:
git add .
git commit -m "short: what changed"
```

Commit criteria:
- **Logical unit of work complete** (function, struct, test, etc.)
- **All validation passes**
- **Code actually works**

### Code Style Preferences

- **Simple over clever**: Readable code beats impressive code
- **Explicit over implicit**: Clear intent over brevity
- **Boring is good**: Standard patterns, no custom macros unless necessary
- **Flat structure**: Avoid deep nesting, prefer early returns
- **Small functions**: 20-30 lines max, single responsibility

### Example Development Flow

```bash
# 1. Add basic struct
# Edit: Add Config struct with 3 fields
[validation cycle]
git add . && git commit -m "config: add basic Config struct"

# 2. Add validation  
# Edit: Add Config::validate() method
[validation cycle]
git add . && git commit -m "config: add validation method"

# 3. Add tests
# Edit: Add 5 unit tests for Config
[validation cycle]
git add . && git commit -m "config: add unit tests"
```

## README Guidelines

Write focused, user-oriented documentation:

**Include**:
- Brief description of what the tool does
- Installation instructions
- Configuration examples
- Usage examples
- Development setup (for contributors)
- License information

**Avoid**:
- Architecture diagrams
- Test coverage statistics
- Performance benchmarks
- Technical implementation details
- Sections that demonstrate competence rather than provide utility

**Approach**: Show, don't tell. Each feature includes example commands. Designed for users who want to understand quickly.
