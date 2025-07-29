# JSON V3 Object Serialization Implementation Guide

## Overview

The ClickHouse JSON type supports multiple serialization formats:
- **V0 (Deprecated Object)**: Legacy format, used for older servers
- **V1 (String)**: Simple string serialization - JSON is sent as raw strings
- **V3 (Object)**: Efficient columnar format that decomposes JSON into paths and typed columns

Currently, our Rust implementation only supports V1 (string) serialization because V3 requires architectural changes to handle the two-phase serialization protocol.

## The Two-Phase Serialization Problem

### How ClickHouse Native Protocol Works

The ClickHouse native protocol serializes data in two distinct phases:

1. **Prefix Phase** (`serialize_prefix`/`WriteStatePrefix`): Writes metadata and headers
2. **Data Phase** (`serialize_column`/`Encode`): Writes the actual column data

For most types, this separation is straightforward. However, JSON V3 requires writing complex metadata that depends on the actual data values.

### The Challenge

JSON V3 format requires writing the following during the **prefix phase**:
1. Version number (8 bytes)
2. Total number of dynamic paths (VarInt)
3. Path names (array of strings)
4. Type metadata for each path (Dynamic column headers)

But our current architecture only has access to the data during the **data phase**. We can't analyze the JSON values to extract paths and types during the prefix phase.

## How Go Implementation Solves This

The Go implementation uses a stateful approach with three phases:

### 1. Append Phase (Data Collection)
```go
func (c *JSON) AppendRow(v any) error {
    // Convert value to internal JSON representation
    obj := convertToJSON(v)
    
    // Extract all paths from the JSON object
    valuesByPath := obj.ValuesByPath()
    
    // For each path, either:
    // - Append to existing dynamic column if path exists
    // - Create new dynamic column for new path
    
    // This builds up c.dynamicPaths and c.dynamicColumns
    // which store the metadata needed for serialization
}
```

### 2. Prefix Phase (Write Metadata)
```go
func (c *JSON) WriteStatePrefix(buffer *proto.Buffer) error {
    // Write version
    buffer.PutUInt64(JSONObjectSerializationVersion)
    
    // Write total dynamic paths (collected during Append)
    buffer.PutUVarInt(uint64(c.totalDynamicPaths))
    
    // Write path names (collected during Append)
    for _, dynamicPath := range c.dynamicPaths {
        buffer.PutString(dynamicPath)
    }
    
    // Write Dynamic column headers for each path
    for _, col := range c.dynamicColumns {
        col.WriteStatePrefix(buffer)
    }
}
```

### 3. Data Phase (Write Column Data)
```go
func (c *JSON) Encode(buffer *proto.Buffer) {
    // Write actual data for each dynamic column
    for _, col := range c.dynamicColumns {
        col.Encode(buffer)
    }
}
```

## Current Rust Architecture Limitations

Our Rust implementation follows a functional, stateless pattern:

```rust
impl Serializer for JsonSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // We only have type information here, not the actual values
        // Can only write the version number
    }

    async fn write<W: ClickHouseWrite>(
        _type_: &Type,
        values: Vec<Value>,  // Data is only available here
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // By the time we have values, it's too late to write the header
    }
}
```

## Required Changes for V3 Support

### Option 1: Thread-Local State (Recommended)

Similar to how we handle Dynamic type serialization, use thread-local storage to maintain state between phases:

```rust
// Thread-local storage for JSON metadata
thread_local! {
    static JSON_METADATA: RefCell<Option<JsonMetadata>> = RefCell::new(None);
}

struct JsonMetadata {
    paths: Vec<String>,
    type_map: HashMap<String, Vec<(usize, Value)>>,
    // ... other metadata
}

impl JsonSerializer {
    // New method to analyze values before serialization
    pub fn prepare_serialization(values: &[Value]) -> Result<()> {
        let metadata = analyze_json_values(values)?;
        JSON_METADATA.with(|m| {
            *m.borrow_mut() = Some(metadata);
        });
        Ok(())
    }
    
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let version = if Self::supports_flat_dynamic_json(state) {
            JSON_OBJECT_SERIALIZATION_VERSION
        } else {
            JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION
        };
        writer.write_u64_le(version).await?;
        
        if version == JSON_OBJECT_SERIALIZATION_VERSION {
            // Retrieve metadata from thread-local storage
            JSON_METADATA.with(|m| {
                let metadata = m.borrow();
                let metadata = metadata.as_ref()
                    .ok_or_else(|| Error::SerializeError(
                        "JSON metadata not prepared".to_string()
                    ))?;
                
                // Write header using metadata
                writer.write_var_uint(metadata.paths.len() as u64).await?;
                for path in &metadata.paths {
                    writer.write_string(path.as_bytes().to_vec()).await?;
                }
                // ... write Dynamic headers for each path
                
                Ok(())
            })?
        }
        
        Ok(())
    }
}
```

### Option 2: Stateful Serializer

Create a stateful JSON serializer that maintains metadata between calls:

```rust
pub struct JsonColumnSerializer {
    version: u64,
    paths: Vec<String>,
    dynamic_columns: Vec<DynamicColumnData>,
    // ... other state
}

impl JsonColumnSerializer {
    pub fn new(server_version: Option<(u64, u64, u64)>) -> Self {
        // Initialize based on server version
    }
    
    pub fn analyze_values(&mut self, values: &[Value]) -> Result<()> {
        // Analyze JSON values and populate paths, types, etc.
    }
    
    pub async fn write_prefix<W: ClickHouseWrite>(&self, writer: &mut W) -> Result<()> {
        // Write version and full header
    }
    
    pub async fn write_data<W: ClickHouseWrite>(&self, writer: &mut W) -> Result<()> {
        // Write column data
    }
}
```

### Option 3: Two-Pass Serialization in Block

Modify the Block serialization to support two-pass processing for complex types:

```rust
impl Block {
    pub async fn write_async<W: ClickHouseWrite>(
        &mut self,
        writer: &mut W,
        revision: u64,
        options: Option<&ClientMetadata>,
    ) -> Result<()> {
        // First pass: prepare complex types
        for (name, type_) in &self.column_types {
            if matches!(type_, Type::JSON) {
                let values = /* extract values for this column */;
                JsonSerializer::prepare_serialization(&values)?;
            }
        }
        
        // Continue with normal serialization...
    }
}
```

## Implementation Steps

1. **Choose Architecture**: Thread-local state (Option 1) is recommended as it's consistent with Dynamic type handling

2. **Implement JSON Analysis**:
   ```rust
   fn analyze_json_values(values: &[Value]) -> Result<JsonMetadata> {
       let mut paths: BTreeMap<String, Vec<Value>> = BTreeMap::new();
       
       for (row_idx, value) in values.iter().enumerate() {
           match value {
               Value::String(json_bytes) => {
                   let json: serde_json::Value = serde_json::from_slice(json_bytes)?;
                   extract_paths(&json, "", &mut paths, row_idx, values.len())?;
               }
               Value::Null => { /* Handle null */ }
               _ => return Err(Error::SerializeError("Invalid JSON value".to_string())),
           }
       }
       
       Ok(JsonMetadata { paths: paths.keys().cloned().collect(), ... })
   }
   ```

3. **Update Serialization Flow**:
   - Call `prepare_serialization` before `write_prefix`
   - Write full header in `write_prefix` using stored metadata
   - Write only data in `write` phase

4. **Handle Edge Cases**:
   - Empty JSON objects
   - Null values
   - New paths appearing in later rows (requires backfilling)
   - Maximum dynamic paths limit

5. **Clean Up State**:
   - Clear thread-local state after serialization
   - Handle errors gracefully

## Testing Considerations

1. **Round-trip Tests**: Ensure serialized data can be correctly deserialized
2. **Compatibility Tests**: Test with different server versions
3. **Performance Tests**: V3 should be more efficient than V1 for large JSON
4. **Edge Cases**: 
   - Empty JSON
   - Deeply nested JSON
   - JSON with many unique paths
   - Mixed types for same path across rows

## Performance Implications

V3 format benefits:
- **Columnar compression**: Similar values in same path compress better
- **Type-specific encoding**: Numbers stored as binary, not strings
- **Selective reading**: Can read only specific paths without parsing entire JSON

V3 format costs:
- **Memory overhead**: Need to analyze all values before writing
- **Complexity**: More complex implementation
- **Backfilling**: New paths require writing nulls for previous rows

## Migration Path

1. Keep `FORCE_STRING_SERIALIZATION = true` as default initially
2. Implement V3 support behind a feature flag
3. Test thoroughly with various JSON structures
4. Enable V3 by default once stable
5. Keep V1 as fallback for compatibility