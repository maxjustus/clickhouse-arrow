# Native Streams Architecture Update Plan

## Executive Summary

After deep investigation into ClickHouse's serialization architecture, we've identified that JSON typed paths with complex types (LowCardinality, Variant) fail due to a mismatch in how we structure bulk serialization. The solution doesn't require implementing multiple physical streams, but rather restructuring our serialization methods to match ClickHouse's expected call pattern.

## Problem Analysis

### Current Failure Mode
When inserting JSON with typed paths containing LowCardinality or Variant types, ClickHouse throws:
```
DB::Exception: Invalid version for SerializationLowCardinality key column
```

### Root Cause
1. **ClickHouse's Expectation**: Bulk serialization occurs in three phases:
   - `serializeBinaryBulkStatePrefix`: Writes prefix data (e.g., version headers), creates state
   - `serializeBinaryBulkWithMultipleStreams`: Writes main data using state
   - `serializeBinaryBulkStateSuffix`: Writes any trailing data, cleans up state

2. **Our Current Implementation**: 
   - `serialize_column()` calls `write()` which includes ALL phases in one call
   - No state is maintained between phases
   - We essentially write the complete serialization twice (prefix + full write)

### Key Insight: Single Stream Reality
Despite the name "MultipleStreams", the Native format actually uses a SINGLE binary stream:
```cpp
// From NativeReader.cpp
settings.getter = [&](ISerialization::SubstreamPath) -> ReadBuffer * { return &istr; };
```

The "substreams" are logical markers, not physical stream separations. All data is written sequentially to the same binary stream.

## Proposed Solution

### Phase 1: Refactor Serialization Architecture

#### 1.1 Create New Trait Methods
Add bulk serialization methods to our serialization trait:
```rust
trait Serializer {
    // Existing methods
    fn write_prefix(...) -> Result<()>;
    fn write(...) -> Result<()>;
    
    // New bulk serialization methods
    fn write_bulk_prefix(
        type_: &Type,
        values: &[Value],  // Needed for analysis
        writer: &mut W,
        state: &mut SerializerState
    ) -> Result<()>;
    
    fn write_bulk_data(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState
    ) -> Result<()>;
    
    fn write_bulk_suffix(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState
    ) -> Result<()> {
        Ok(()) // Default no-op, most types don't need suffix
    }
}
```

#### 1.2 Default Implementation
For simple types that don't need state:
```rust
impl<T: Serializer> BulkSerializerDefaults for T {
    fn write_bulk_prefix(...) -> Result<()> {
        // Default: do nothing
        Ok(())
    }
    
    fn write_bulk_data(...) -> Result<()> {
        // Default: call existing write() method
        self.write(type_, values, writer, state)
    }
}
```

### Phase 2: Implement for Complex Types

#### 2.1 LowCardinality Implementation
```rust
impl LowCardinalitySerializer {
    fn write_bulk_prefix(
        type_: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState
    ) -> Result<()> {
        // Write version
        writer.write_u64_le(LOW_CARDINALITY_VERSION).await?;
        
        // Analyze values and build dictionary
        let dictionary = build_dictionary(values);
        
        // Store in state for use in write_bulk_data
        state.type_specific = TypeSpecificState::LowCardinality(LowCardState {
            dictionary,
            // ... other state
        });
        
        Ok(())
    }
    
    fn write_bulk_data(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState
    ) -> Result<()> {
        // Retrieve dictionary from state
        let low_card_state = state.type_specific.as_low_cardinality()?;
        
        // Write flags
        writer.write_u64_le(flags).await?;
        
        // Write dictionary
        writer.write_u64_le(low_card_state.dictionary.len()).await?;
        inner_type.serialize_column(low_card_state.dictionary, writer, state).await?;
        
        // Write indexes
        writer.write_u64_le(values.len()).await?;
        for value in values {
            let index = low_card_state.dictionary.index_of(value);
            writer.write_index(index).await?;
        }
        
        Ok(())
    }
}
```

#### 2.2 Variant Implementation
Similar pattern - version and type prefixes in `write_bulk_prefix`, discriminators and data in `write_bulk_data`.

### Phase 3: Update JSON Serialization

#### 3.1 Modify JSON typed path serialization
```rust
// In json.rs write method
for (path, type_) in &typed_paths {
    let column_values = get_column_values(path);
    
    // Analyze values to build state
    let mut typed_state = SerializerState::default();
    
    // Use bulk serialization pattern
    type_.write_bulk_prefix(&column_values, writer, &mut typed_state).await?;
    type_.write_bulk_data(column_values, writer, &mut typed_state).await?;
    type_.write_bulk_suffix(writer, &mut typed_state).await?;
}
```

### Phase 4: Update Type dispatch

#### 4.1 Add new dispatch methods to Type
```rust
impl Type {
    pub fn write_bulk_prefix(&self, values: &[Value], writer: &mut W, state: &mut SerializerState) -> Result<()> {
        match self {
            Type::LowCardinality(_) => {
                LowCardinalitySerializer::write_bulk_prefix(self, values, writer, state)
            }
            Type::Variant(_) => {
                VariantSerializer::write_bulk_prefix(self, values, writer, state)
            }
            _ => Ok(()) // Most types don't need prefix
        }
    }
    
    pub fn write_bulk_data(&self, values: Vec<Value>, writer: &mut W, state: &mut SerializerState) -> Result<()> {
        match self {
            Type::LowCardinality(_) => {
                LowCardinalitySerializer::write_bulk_data(self, values, writer, state)
            }
            Type::Variant(_) => {
                VariantSerializer::write_bulk_data(self, values, writer, state)
            }
            _ => {
                // Default: use existing serialize_column which calls write()
                self.serialize_column(values, writer, state).await
            }
        }
    }
}
```

## Implementation Strategy

### Step 1: Foundation (2-3 days)
- [ ] Add new trait methods with default implementations
- [ ] Update SerializerState to support type-specific state storage
- [ ] Add tests for state management

### Step 2: LowCardinality Support (2-3 days)
- [ ] Implement write_bulk_prefix for LowCardinality
- [ ] Implement write_bulk_data for LowCardinality
- [ ] Update JSON serializer to use new methods
- [ ] Test with existing example

### Step 3: Variant Support (2-3 days)
- [ ] Implement write_bulk_prefix for Variant
- [ ] Implement write_bulk_data for Variant
- [ ] Test with Variant typed paths

### Step 4: Cleanup and Optimization (1-2 days)
- [ ] Remove temporary helper methods (write_data, write_data_sync)
- [ ] Optimize state storage
- [ ] Add comprehensive tests

## Backward Compatibility

This change maintains full backward compatibility:
1. Existing types that don't implement the new methods will use defaults
2. The wire format remains unchanged
3. Only the internal call pattern changes for complex types

## Alternative Approach (If Needed)

If the bulk serialization approach proves too complex, we could:
1. Detect LowCardinality/Variant in typed paths
2. Return a clear error message explaining the limitation
3. Document supported types for typed paths
4. Wait for future architecture updates to support these types

## Success Criteria

1. LowCardinality types work in JSON typed paths
2. Variant types work in JSON typed paths  
3. No regression in existing functionality
4. Performance remains comparable
5. Tests pass with ClickHouse 24.x and 25.x

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| State management complexity | High | Start with LowCardinality only, add Variant after proven approach |
| Wire format mismatch | High | Use protocol analyzer script to verify output matches ClickHouse |
| Performance regression | Medium | Benchmark before/after, optimize hot paths |
| Breaking existing code | High | Extensive test coverage, gradual rollout |

## Conclusion

This plan addresses the root cause of the LowCardinality/Variant serialization issues without requiring a complete rewrite of our streaming architecture. By separating the bulk serialization phases and maintaining proper state, we can match ClickHouse's expectations while keeping our single-stream model.

The implementation is incremental, testable, and maintains backward compatibility. If successful, this will enable full support for all ClickHouse types in JSON typed paths.