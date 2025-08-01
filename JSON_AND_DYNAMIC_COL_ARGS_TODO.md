# JSON and Dynamic Column Arguments Implementation TODO

## Problem Statement

ClickHouse supports parameterized Dynamic and JSON types, but clickhouse-arrow currently treats them as simple, parameterless types. This creates a compatibility gap with ClickHouse servers and other clients (like clickhouse-go) that properly handle these parameters.

## Current State

**ClickHouse Supports:**
```sql
CREATE TABLE test (
    dyn_col Dynamic(max_types=16),
    json_col JSON(max_dynamic_paths=100, max_dynamic_types=8)
);
```

**clickhouse-arrow Currently:**
```rust
// No parameter support
Type::Dynamic
Type::JSON
```

## Parameters and Their Meaning

### Dynamic Type Parameters
- **Syntax**: `Dynamic(max_types=N)` where `0 <= N <= 254`
- **Default**: `max_types=32`
- **Purpose**: Limits how many different data types can be stored as separate subcolumns
- **Impact**: Affects storage efficiency and query performance

### JSON Type Parameters
- **Syntax**: `JSON(max_dynamic_paths=N, max_dynamic_types=M, ...)`
- **Parameters**:
  - `max_dynamic_paths` (default: 1024) - limits JSON key paths stored as subcolumns
  - `max_dynamic_types` (default: 32) - limits data types per JSON key path
  - Additional parameters for type hints and path skipping patterns
- **Impact**: Critical for performance with complex JSON structures

## Required Settings
- `allow_experimental_dynamic_type = 1` (for Dynamic type)
- `allow_experimental_json_type = 1` (for JSON type)

## Implementation Plan

### Phase 1: Research clickhouse-go Implementation 🔍 ✅
- [x] Download and analyze clickhouse-go source code
- [x] Understand how they parse `Dynamic(max_types=N)` syntax
- [x] Understand how they parse `JSON(max_dynamic_paths=N, max_dynamic_types=M)` syntax  
- [x] Identify their type representation strategy
- [x] Document their approach for wire protocol handling

**Key Findings from clickhouse-go:**
- Uses parameter extraction from type strings (e.g., `Dynamic(max_types=10)` → extract `max_types=10`)
- Maintains version compatibility: Dynamic v3/v1, JSON with deprecated versions
- Two-layer API: low-level `/lib/column/` + high-level `/lib/chcol/`
- Per-column parameter parsing, not centralized
- Default values: Dynamic max_types=32, JSON max_dynamic_paths=1024
- Supports complex JSON syntax: `JSON('path' Type, SKIP 'other', max_dynamic_paths=N)`

### Phase 2: Update Type System 🔧
- [ ] Modify `Type` enum to support parameters:
  ```rust
  pub enum Type {
      Dynamic(Option<u32>), // max_types parameter
      JSON {
          max_dynamic_paths: Option<u32>,
          max_dynamic_types: Option<u32>,
          // TODO: Add support for additional parameters
      },
  }
  ```
- [ ] Update `Display` implementation to format parameterized types correctly
- [ ] Update `Clone`, `Debug`, `PartialEq` derives to handle new structure

### Phase 3: Update Parser 📝
- [ ] Extend `FromStr` implementation in `types/deserialize.rs`
- [ ] Handle `Dynamic()` and `Dynamic(max_types=N)` parsing
- [ ] Handle `JSON()` and `JSON(param1=N, param2=M)` parsing
- [ ] Add comprehensive parsing tests for edge cases
- [ ] Ensure backwards compatibility with parameter-less syntax

### Phase 4: Update Serialization/Deserialization 🔄
- [ ] Update Dynamic serialization to handle parameters
- [ ] Update JSON serialization to handle parameters  
- [ ] Update deserialization to properly read parameterized types from wire protocol
- [ ] Test round-trip compatibility with ClickHouse server

### Phase 5: Testing & Validation ✅
- [ ] Add unit tests for parameterized type parsing
- [ ] Add integration tests with real ClickHouse server
- [ ] Test compatibility with different parameter combinations
- [ ] Test edge cases (max values, invalid parameters)
- [ ] Ensure compatibility with existing non-parameterized usage

### Phase 6: Documentation 📚
- [ ] Update API documentation
- [ ] Add examples of parameterized Dynamic/JSON usage
- [ ] Document performance implications of different parameter values
- [ ] Update migration guide for users

## Files That Need Updates

### Core Type System
- `clickhouse-arrow/src/native/types.rs` - Type enum and Display implementation
- `clickhouse-arrow/src/native/types/deserialize.rs` - FromStr parsing logic

### Serialization/Deserialization  
- `clickhouse-arrow/src/native/types/serialize/dynamic.rs` - Dynamic serialization
- `clickhouse-arrow/src/native/types/serialize/json.rs` - JSON serialization
- `clickhouse-arrow/src/native/types/deserialize/dynamic.rs` - Dynamic deserialization
- `clickhouse-arrow/src/native/types/deserialize/json.rs` - JSON deserialization

### Tests
- `clickhouse-arrow/tests/tests/native.rs` - Integration tests
- Unit test files for each affected module

## Compatibility Considerations

1. **Backwards Compatibility**: Must support existing `Dynamic` and `JSON` without parameters
2. **Default Values**: Should match ClickHouse defaults (Dynamic: max_types=32, JSON: max_dynamic_paths=1024, max_dynamic_types=32)
3. **Wire Protocol**: Ensure parameters are properly serialized/deserialized over the wire
4. **Server Compatibility**: Test with different ClickHouse versions (24.x, 25.x)

## Risk Assessment

### High Risk
- Breaking changes to existing Dynamic/JSON usage
- Wire protocol compatibility issues
- Performance impact of parameter handling

### Medium Risk  
- Parser complexity with nested parameter syntax
- Edge cases with invalid parameter combinations
- Memory usage increase with parameter storage

### Low Risk
- Documentation updates
- Test coverage expansion

## Success Criteria

✅ **Complete when:**
1. Can parse and represent `Dynamic(max_types=16)` correctly
2. Can parse and represent `JSON(max_dynamic_paths=100, max_dynamic_types=8)` correctly
3. Round-trip serialization/deserialization works with parameters
4. All existing tests pass (backwards compatibility)
5. New comprehensive test suite passes
6. Integration tests work with real ClickHouse server using parameterized types

## Research Status

- [x] Identified the gap in current implementation
- [x] Documented ClickHouse parameter syntax and semantics
- [ ] **IN PROGRESS**: Analyzing clickhouse-go implementation approach
- [ ] **NEXT**: Design optimal type system changes