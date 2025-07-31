# Dynamic Type v1/v2 Serialization Implementation Plan

This document serves as the single source of truth for implementing Dynamic type v1 and v2 serialization formats in the clickhouse-arrow Rust client.

## Overview

The Dynamic type in ClickHouse has three serialization versions:
- **v1**: Legacy format with `max_dynamic_types` parameter (for servers < 24.11)
- **v2**: Current default without `max_dynamic_types` (for servers >= 24.11 and < 25.6)
- **v3**: Flattened format with indexed columns (for servers >= 25.6) - already implemented

## Version Selection Logic

```rust
fn get_dynamic_version(server_version: Option<(u64, u64, u64)>) -> u64 {
    match server_version {
        Some((major, minor, _)) => {
            if major < 24 || (major == 24 && minor < 11) {
                DYNAMIC_VERSION_V1  // 1
            } else if major < 25 || (major == 25 && minor < 6) {
                DYNAMIC_VERSION_V2  // 2
            } else {
                DYNAMIC_VERSION_V3  // 3
            }
        }
        None => DYNAMIC_VERSION_V3  // Default to latest
    }
}
```

## Format Specifications

### Dynamic v1 Format (Legacy)

**Prefix (DynamicStructure stream):**
```
1. VarUInt(1)                    // Version
2. VarUInt(max_dynamic_types)    // Default: 32
3. VarUInt(num_dynamic_types)    // Actual number of types
4. For each type:
   - DataType serialization      // Full type definition
```

**Data (DynamicData stream):**
- Standard Variant serialization (see Variant format below)
- 8-bit discriminators
- NULL discriminator = 255 (0xFF)

### Dynamic v2 Format (Current Default)

**Prefix (DynamicStructure stream):**
```
1. VarUInt(2)                    // Version
2. VarUInt(num_dynamic_types)    // Number of types
3. For each type:
   - DataType serialization      // Full type definition
```

**Data (DynamicData stream):**
- Same as v1 (standard Variant serialization)

### Dynamic v3 Format (Flattened) - Already Implemented

**Prefix:**
```
1. VarUInt(3)                    // Version
2. VarUInt(num_types)            // Number of types
3. For each type:
   - String(type_name)           // Type name as string (not DataType)
```

**Data:**
- Dynamic-sized discriminators based on type count
- Separate columns for each type
- NULL discriminator = total_types

## Key Implementation Details

### 1. Constants to Add

```rust
const DYNAMIC_VERSION_V1: u64 = 1;  // Legacy with max_dynamic_types
const DYNAMIC_VERSION_V2: u64 = 2;  // Without max_dynamic_types
const DYNAMIC_VERSION_V3: u64 = 3;  // Flattened (current implementation)
const DEFAULT_MAX_DYNAMIC_TYPES: u64 = 32;  // For v1
```

### 2. DataType Serialization

For v1/v2, types must be serialized as full DataType definitions, not just strings:

```rust
// Example: "Array(Int32)" serialization
writer.write_string("Array").await?;     // Type name
writer.write_string("Int32").await?;     // Nested type

// Example: "Nullable(String)" serialization  
writer.write_string("Nullable").await?;  // Type name
writer.write_string("String").await?;    // Nested type
```

### 3. Variant Data Format for v1/v2

Both v1 and v2 use standard Variant serialization:
- Discriminators: Array of UInt8 values
- NULL discriminator: 255 (0xFF)
- Column data: Serialized in discriminator order (alphabetically sorted types)
- No granule-based compression (BASIC mode only)

### 4. Type Sorting

Types must be sorted alphabetically to determine discriminator values:
```rust
let mut types = vec!["String", "Int32", "Float64"];
types.sort();  // Results in: ["Float64", "Int32", "String"]
// Discriminators: Float64=0, Int32=1, String=2, NULL=255
```

## Implementation Steps

### Step 1: Update dynamic.rs Constants

```rust
// In src/native/types/serialize/dynamic.rs
const DYNAMIC_VERSION_V1: u64 = 1;
const DYNAMIC_VERSION_V2: u64 = 2;
const DYNAMIC_VERSION_V3: u64 = 3;
const DEFAULT_MAX_DYNAMIC_TYPES: u64 = 32;
```

### Step 2: Add Version Detection

```rust
impl DynamicSerializer {
    fn get_version(state: &SerializerState) -> u64 {
        if let Some((major, minor, _)) = state.server_version {
            if major < 24 || (major == 24 && minor < 11) {
                DYNAMIC_VERSION_V1
            } else if major < 25 || (major == 25 && minor < 6) {
                DYNAMIC_VERSION_V2
            } else {
                DYNAMIC_VERSION_V3
            }
        } else {
            DYNAMIC_VERSION_V3
        }
    }
}
```

### Step 3: Implement DataType Serialization

```rust
async fn write_data_type<W: ClickHouseWrite>(
    type_: &Type,
    writer: &mut W,
) -> Result<()> {
    match type_ {
        Type::String => {
            writer.write_string("String").await?;
        }
        Type::UInt64 => {
            writer.write_string("UInt64").await?;
        }
        Type::Array(inner) => {
            writer.write_string("Array").await?;
            write_data_type(inner, writer).await?;
        }
        Type::Nullable(inner) => {
            writer.write_string("Nullable").await?;
            write_data_type(inner, writer).await?;
        }
        // ... handle all types
    }
    Ok(())
}
```

### Step 4: Update write_prefix

```rust
pub(crate) async fn write_prefix<W: ClickHouseWrite>(
    _type: &Type,
    writer: &mut W,
    state: &mut SerializerState,
) -> Result<()> {
    let version = Self::get_version(state);
    writer.write_var_uint(version).await?;
    
    if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
        match version {
            DYNAMIC_VERSION_V1 => {
                // Write max_dynamic_types
                writer.write_var_uint(DEFAULT_MAX_DYNAMIC_TYPES).await?;
                // Write num_dynamic_types
                writer.write_var_uint(dynamic_state.type_names.len() as u64).await?;
                // Write DataType definitions
                for type_name in &dynamic_state.type_names {
                    let (_, typ) = &dynamic_state.type_map[type_name];
                    write_data_type(typ, writer).await?;
                }
            }
            DYNAMIC_VERSION_V2 => {
                // Write num_dynamic_types
                writer.write_var_uint(dynamic_state.type_names.len() as u64).await?;
                // Write DataType definitions
                for type_name in &dynamic_state.type_names {
                    let (_, typ) = &dynamic_state.type_map[type_name];
                    write_data_type(typ, writer).await?;
                }
            }
            DYNAMIC_VERSION_V3 => {
                // Existing v3 implementation
                writer.write_var_uint(dynamic_state.total_types).await?;
                for type_name in &dynamic_state.type_names {
                    writer.write_string(type_name).await?;
                }
                // Write nested prefixes...
            }
            _ => unreachable!()
        }
    }
    Ok(())
}
```

### Step 5: Update Data Serialization

```rust
pub(crate) async fn write<W: ClickHouseWrite>(
    _type: &Type,
    values: &[Value],
    writer: &mut W,
    state: &mut SerializerState,
) -> Result<()> {
    let version = Self::get_version(state);
    
    match version {
        DYNAMIC_VERSION_V1 | DYNAMIC_VERSION_V2 => {
            // Use Variant serialization
            // 1. Write discriminators as UInt8 array
            // 2. Write column data for each type
            Self::write_variant_data(values, writer, state).await?;
        }
        DYNAMIC_VERSION_V3 => {
            // Existing v3 implementation with dynamic discriminators
            // ...
        }
        _ => unreachable!()
    }
    Ok(())
}
```

### Step 6: Implement Variant-style Data Writing

```rust
async fn write_variant_data<W: ClickHouseWrite>(
    values: &[Value],
    writer: &mut W,
    state: &mut SerializerState,
) -> Result<()> {
    let (type_names, type_map, _) = // ... from state
    
    // Write discriminators (UInt8 array)
    for value in values {
        if matches!(value, Value::Null) {
            writer.write_u8(255).await?;  // NULL discriminator
        } else {
            let type_name = value.guess_type().to_string();
            let (type_idx, _) = &type_map[&type_name];
            writer.write_u8(*type_idx as u8).await?;
        }
    }
    
    // Write column data for each type
    for (type_idx, type_name) in type_names.iter().enumerate() {
        let values_for_type: Vec<Value> = // ... collect values for this type
        if !values_for_type.is_empty() {
            let (_, typ) = &type_map[type_name];
            typ.serialize_column(values_for_type, writer, state).await?;
        }
    }
    Ok(())
}
```

## JSON/Object Serialization Updates

JSON type uses Object serialization with three versions based on server compatibility:

### Object Version Selection

```rust
fn get_serialization_version(state: &SerializerState) -> u64 {
    if let Some((major, minor, _)) = state.server_version {
        if major < 24 || (major == 24 && minor < 11) {
            JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION  // 0
        } else if major < 25 || (major == 25 && minor < 6) {
            JSON_OBJECT_SERIALIZATION_VERSION_V2  // 2
        } else {
            JSON_OBJECT_SERIALIZATION_VERSION  // 3
        }
    } else {
        JSON_OBJECT_SERIALIZATION_VERSION  // Default to v3
    }
}
```

### Object v0 Format (Legacy)

**Prefix (ObjectStructure stream):**
```
1. VarUInt(0)                        // Version
2. VarUInt(max_dynamic_paths)        // Default: 1024
3. VarUInt(num_typed_paths)          // Number of typed paths (usually 0 for JSON)
4. For each typed path:
   - String(path_name)               // e.g., "user.name"
   - DataType serialization          // Type definition
5. VarUInt(num_dynamic_paths)        // Number of dynamic paths
6. For each dynamic path:
   - String(path_name)               // e.g., "user.age"
```

**Data:**
- For each typed path: Column data
- For all dynamic paths: One Dynamic column (v1 or v2 format based on server)
- SharedData: VarUInt(0) per row (empty shared data)

### Object v2 Format (Current Default)

**Prefix (ObjectStructure stream):**
```
1. VarUInt(2)                        // Version
2. VarUInt(num_typed_paths)          // Number of typed paths (usually 0)
3. For each typed path:
   - String(path_name)
   - DataType serialization
4. VarUInt(num_dynamic_paths)        // Number of dynamic paths
5. For each dynamic path:
   - String(path_name)
```

**Data:**
- Same as v0 but without SharedData stream

### Object v3 Format (Flattened) - Already Implemented

**Prefix:**
```
1. VarUInt(3)                        // Version
2. VarUInt(num_paths)                // Total number of paths
3. For each path:
   - String(path_name)
   - DataType serialization
```

**Data:**
- One column per path (no Dynamic column)

### Constants to Add for JSON

```rust
// In src/native/types/serialize/json.rs
const JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION: u64 = 0;  // Already exists
const JSON_OBJECT_SERIALIZATION_VERSION_V2: u64 = 2;         // Add this
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;            // Already exists
const DEFAULT_MAX_DYNAMIC_PATHS: u64 = 1024;                 // Already exists
```

### Key JSON Implementation Details

1. **Path Discovery**: JSON values are parsed and flattened into paths:
   ```json
   {"user": {"name": "Alice", "age": 30}} 
   ```
   Becomes paths: `user.name` (String), `user.age` (Int64)

2. **Dynamic Column**: For v0/v2, all dynamic paths share one Dynamic column:
   - The Dynamic column itself uses v1/v2 format based on server version
   - Each path's values are collected and serialized in the Dynamic column

3. **Path Ordering**: Paths must be sorted alphabetically in the prefix

4. **Type Inference**: JSON values are mapped to ClickHouse types:
   - JSON null → NULL
   - JSON boolean → UInt8 (0 or 1)
   - JSON number → Int64, UInt64, or Float64
   - JSON string → String
   - JSON array/object → String (serialized JSON)

### Implementation Steps for JSON

1. **Update version detection** in `json.rs`
2. **Modify write_paths_header** to handle v0 vs v2 format
3. **Update Dynamic header writing** to use appropriate Dynamic version
4. **Handle SharedData** for v0 (write empty VarUInt(0) per row)
5. **Ensure path sorting** is consistent

## Testing Requirements

1. **Unit Tests**: Test version selection logic with different server versions
2. **Round-trip Tests**: Ensure v1/v2 data can be serialized and deserialized
3. **Wire Format Tests**: Verify exact byte sequences for v1/v2 formats
4. **Compatibility Tests**: Test against actual ClickHouse servers of different versions
5. **Type Coverage**: Test all supported types in Dynamic columns

## Files to Modify

1. `src/native/types/serialize/dynamic.rs` - Main implementation
2. `src/native/types/serialize/json.rs` - Update version detection
3. `src/formats.rs` - Potentially update state structures if needed
4. `src/native/types/mod.rs` - Add DataType serialization helper
5. Tests in `tests/` directory

## Important Notes

1. **DataType vs String**: v1/v2 use full DataType serialization, v3 uses simple strings
2. **NULL Handling**: v1/v2 use discriminator 255, v3 uses total_types
3. **Discriminator Size**: v1/v2 always use UInt8, v3 uses dynamic sizing
4. **Type Ordering**: Always sort types alphabetically for consistent discriminators
5. **Backward Compatibility**: Ensure existing v3 code continues to work unchanged