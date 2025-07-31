# Dynamic Type v1/v2 and JSON Implementation Todo List

This file tracks the implementation tasks for adding Dynamic v1/v2 and JSON v0/v2 serialization support.

## Phase 1: Dynamic V2 Implementation (High Priority)
**Target Server: ClickHouse 25.1 (supports v2)**

- [ ] **DYNAMIC V2: Add DYNAMIC_VERSION_V2 constant and server version detection for 24.11-25.5**
  - Add `const DYNAMIC_VERSION_V2: u64 = 2;` to dynamic.rs
  - Implement `get_version()` method with server version checks
  
- [ ] **DYNAMIC V2: Implement write_data_type() method for DataType serialization**
  - Create helper to serialize Type enum as DataType format
  - Handle nested types (Array, Nullable, etc.)
  
- [ ] **DYNAMIC V2: Update write_prefix to handle v2 format (no max_dynamic_types)**
  - Add v2 branch in write_prefix
  - Write version, num_types, then DataType definitions
  
- [ ] **DYNAMIC V2: Implement write_variant_data() for v2 using 8-bit discriminators**
  - Write discriminators as UInt8 array
  - Write column data for each type in order
  - NULL discriminator = 255
  
- [ ] **DYNAMIC V2: Update write() method to use variant serialization for v2**
  - Add version check and branch to variant serialization
  - Reuse existing type grouping logic
  
- [ ] **DYNAMIC V2: Add unit tests for v2 format**
  - Test wire format matches expected bytes
  - Test round-trip serialization
  - Test various type combinations
  
- [ ] **DYNAMIC V2: Test against ClickHouse 25.1 server (supports v2)**
  - Run integration tests with CLICKHOUSE_VERSION=25.1
  - Verify data correctness

## Phase 2: Dynamic V1 Implementation (Medium Priority)
**Target Server: ClickHouse 24.8 (requires v1)**

- [ ] **DYNAMIC V1: Add DYNAMIC_VERSION_V1 and DEFAULT_MAX_DYNAMIC_TYPES constants**
  - Add `const DYNAMIC_VERSION_V1: u64 = 1;`
  - Add `const DEFAULT_MAX_DYNAMIC_TYPES: u64 = 32;`
  
- [ ] **DYNAMIC V1: Update write_prefix to handle v1 format (with max_dynamic_types)**
  - Add v1 branch in write_prefix
  - Write version, max_dynamic_types, num_types, then DataType definitions
  
- [ ] **DYNAMIC V1: Reuse v2 data serialization (same Variant format)**
  - v1 and v2 share the same data format
  - Only prefix differs
  
- [ ] **DYNAMIC V1: Add unit tests for v1 format**
  - Test max_dynamic_types is written correctly
  - Test wire format matches expected bytes
  
- [ ] **DYNAMIC V1: Test against ClickHouse 24.8 server (requires v1)**
  - Run integration tests with CLICKHOUSE_VERSION=24.8
  - Verify backward compatibility

## Phase 3: JSON V2 Implementation (Medium Priority)
**Target Server: ClickHouse 25.1**

- [ ] **JSON V2: Add JSON_OBJECT_SERIALIZATION_VERSION_V2 constant**
  - Add `const JSON_OBJECT_SERIALIZATION_VERSION_V2: u64 = 2;`
  
- [ ] **JSON V2: Update get_serialization_version for 24.11-25.5 servers**
  - Modify existing version detection logic
  - Return v2 for appropriate server range
  
- [ ] **JSON V2: Update write_paths_header to skip max_dynamic_paths for v2**
  - Add conditional logic in write_paths_header_async/sync
  - v2 doesn't include max_dynamic_paths parameter
  
- [ ] **JSON V2: Ensure Dynamic column uses appropriate version in v2**
  - Dynamic column should use Dynamic v2 format
  - Pass through server version to Dynamic serialization
  
- [ ] **JSON V2: Add unit tests for JSON v2 format**
  - Test path discovery and sorting
  - Test Dynamic column format
  - Test round-trip serialization
  
- [ ] **JSON V2: Test against ClickHouse 25.1 server**
  - Run integration tests
  - Verify JSON parsing and reconstruction

## Phase 4: JSON V0 Implementation (Low Priority)
**Target Server: ClickHouse 24.8**

- [ ] **JSON V0: Update version detection for < 24.11 servers**
  - Return v0 for servers < 24.11
  
- [ ] **JSON V0: Ensure write_paths_header includes max_dynamic_paths for v0**
  - Write DEFAULT_MAX_DYNAMIC_PATHS (1024) after version
  
- [ ] **JSON V0: Add SharedData writing (VarUInt(0) per row) for v0**
  - After all column data, write VarUint(0) for each row
  - This represents empty shared data
  
- [ ] **JSON V0: Ensure Dynamic column uses v1 format for v0**
  - Dynamic column should use Dynamic v1 format
  - Includes max_dynamic_types parameter
  
- [ ] **JSON V0: Add unit tests for JSON v0 format**
  - Test max_dynamic_paths is written
  - Test SharedData stream
  - Test Dynamic v1 format usage
  
- [ ] **JSON V0: Test against ClickHouse 24.8 server**
  - Run integration tests with old server
  - Verify backward compatibility

## Testing Strategy

### Unit Tests
- Wire format verification (exact byte sequences)
- Round-trip tests (serialize → deserialize)
- Edge cases (empty, nulls, many types)

### Integration Tests
- Test against specific ClickHouse versions:
  - 24.8: Requires Dynamic v1, JSON v0
  - 25.1: Supports Dynamic v2, JSON v2
  - 25.6+: Supports Dynamic v3, JSON v3 (already implemented)

### Protocol Debugging with chc-tcp.py
Use the `scripts/chc-tcp.py` tool to capture and analyze ClickHouse protocol traffic:

```bash
# Capture JSON serialization (v3 flattened format) from system ClickHouse
./scripts/chc-tcp.py "SELECT map('a', number)::JSON from system.numbers limit 10 SETTINGS output_format_native_use_flattened_dynamic_and_json_serialization=1" --pcap json-v3.pcap

# Capture JSON serialization (v2 format - disable flattened) from system ClickHouse
./scripts/chc-tcp.py "SELECT map('a', number)::JSON from system.numbers limit 10 SETTINGS output_format_native_use_flattened_dynamic_and_json_serialization=0" --pcap json-v2.pcap

# View hex dump without capturing (no pcap flag)
./scripts/chc-tcp.py "SELECT number::Dynamic from system.numbers limit 3"

# Test Dynamic v1 format with ClickHouse 24.8
./scripts/chc-tcp.py "SELECT number::Dynamic from system.numbers limit 5" --clickhouse-container-version 24.8 --pcap dynamic-v1-ch24.8.pcap

# Test JSON v0 format with ClickHouse 24.8
./scripts/chc-tcp.py "SELECT map('a', number)::JSON from system.numbers limit 5" --clickhouse-container-version 24.8 --pcap json-v0-ch24.8.pcap

# Test Dynamic v2 format with ClickHouse 25.1
./scripts/chc-tcp.py "SELECT number::Dynamic from system.numbers limit 5" --clickhouse-container-version 25.1 --pcap dynamic-v2-ch25.1.pcap
```

This tool creates PCAP files that can be analyzed to understand the exact wire format. Use `--clickhouse-container-version` to test against specific ClickHouse versions via testcontainers. Without `--pcap`, it outputs hex dump format for quick inspection.

### Test Commands
```bash
# Test specific version
CLICKHOUSE_VERSION=25.1 cargo test --features test-utils

# Test all versions
./scripts/test-versions.sh

# Test specific version range
VERSIONS="24.8 25.1" ./scripts/test-versions.sh
```

## Progress Tracking
Mark items with [x] as they are completed. Each phase should be fully completed and tested before moving to the next.