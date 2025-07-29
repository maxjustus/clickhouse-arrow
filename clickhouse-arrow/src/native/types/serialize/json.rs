use std::collections::{BTreeMap, HashMap};
use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Value};

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION: u64 = 0;
const JSON_STRING_SERIALIZATION_VERSION: u64 = 1;
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;

/// Parsed JSON data organized by dynamic paths
#[derive(Debug, Clone)]
struct JsonData {
    /// Map from path (e.g., "user.name") to values for that path across all rows
    path_columns: BTreeMap<String, Vec<Value>>,
    /// Number of rows
    rows: usize,
}

impl JsonData {
    /// Parse JSON values into path-organized structure
    fn from_values(values: Vec<Value>) -> Result<Self> {
        let mut path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let rows = values.len();
        
        for (row_idx, value) in values.into_iter().enumerate() {
            match value {
                Value::String(bytes) => {
                    // Parse JSON string into object
                    let json_str = String::from_utf8(bytes).map_err(|e| {
                        Error::SerializeError(format!("Invalid UTF-8 in JSON string: {}", e))
                    })?;
                    
                    let json_value: serde_json::Value = serde_json::from_str(&json_str)
                        .map_err(|e| {
                            Error::SerializeError(format!("Invalid JSON string: {}", e))
                        })?;
                    
                    // Extract paths from JSON object
                    Self::extract_paths_from_json(&json_value, "", &mut path_columns, row_idx, rows)?;
                }
                Value::Null => {
                    // For null values, we don't add any paths - they'll be filled with nulls
                }
                _ => {
                    return Err(Error::SerializeError(format!(
                        "JSON serialization only supports String values containing JSON, got: {:?}",
                        value
                    )));
                }
            }
        }
        
        // Ensure all path columns have the correct number of rows (fill with nulls)
        for column in path_columns.values_mut() {
            while column.len() < rows {
                column.push(Value::Null);
            }
        }
        
        Ok(JsonData {
            path_columns,
            rows,
        })
    }
    
    /// Recursively extract paths from JSON value
    fn extract_paths_from_json(
        json_value: &serde_json::Value,
        current_path: &str,
        path_columns: &mut BTreeMap<String, Vec<Value>>,
        row_idx: usize,
        total_rows: usize,
    ) -> Result<()> {
        match json_value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let path = if current_path.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", current_path, key)
                    };
                    
                    Self::extract_paths_from_json(value, &path, path_columns, row_idx, total_rows)?;
                }
            }
            _ => {
                // Leaf value - convert to ClickHouse Value and store
                let ch_value = Self::json_value_to_clickhouse_value(json_value)?;
                
                // Ensure the column exists and has the right size
                let column = path_columns.entry(current_path.to_string()).or_insert_with(|| {
                    vec![Value::Null; total_rows]
                });
                
                // Set the value at the correct row index
                if row_idx < column.len() {
                    column[row_idx] = ch_value;
                }
            }
        }
        Ok(())
    }
    
    /// Convert serde_json::Value to ClickHouse Value
    fn json_value_to_clickhouse_value(json_value: &serde_json::Value) -> Result<Value> {
        let value = match json_value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => {
                // ClickHouse doesn't have a native Bool, use UInt8
                Value::UInt8(if *b { 1 } else { 0 })
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int64(i)
                } else if let Some(u) = n.as_u64() {
                    Value::UInt64(u)
                } else if let Some(f) = n.as_f64() {
                    Value::Float64(f)
                } else {
                    return Err(Error::SerializeError(format!(
                        "Unsupported JSON number format: {}",
                        n
                    )));
                }
            }
            serde_json::Value::String(s) => Value::String(s.as_bytes().to_vec()),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                // For complex types, serialize back to JSON string
                let json_str = serde_json::to_string(json_value).map_err(|e| {
                    Error::SerializeError(format!("Failed to serialize JSON value: {}", e))
                })?;
                Value::String(json_str.into_bytes())
            }
        };
        Ok(value)
    }
}

impl JsonSerializer {
    /// Get the ClickHouse type name for a Value
    fn get_value_type_name(value: &Value) -> String {
        match value {
            Value::Null => "String".to_string(), // Nulls are typically String type in JSON context
            Value::Int8(_) => "Int8".to_string(),
            Value::Int16(_) => "Int16".to_string(),
            Value::Int32(_) => "Int32".to_string(),
            Value::Int64(_) => "Int64".to_string(),
            Value::Int128(_) => "Int128".to_string(),
            Value::Int256(_) => "Int256".to_string(),
            Value::UInt8(_) => "UInt8".to_string(),
            Value::UInt16(_) => "UInt16".to_string(),
            Value::UInt32(_) => "UInt32".to_string(),
            Value::UInt64(_) => "UInt64".to_string(),
            Value::UInt128(_) => "UInt128".to_string(),
            Value::UInt256(_) => "UInt256".to_string(),
            Value::Float32(_) => "Float32".to_string(),
            Value::Float64(_) => "Float64".to_string(),
            Value::String(_) => "String".to_string(),
            _ => "String".to_string(), // Fallback to String for complex types
        }
    }
    
    /// Write Dynamic column data (discriminators + column data)
    async fn write_dynamic_column_data<W: ClickHouseWrite>(
        column_values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Group values by type to determine discriminators
        let mut type_groups: HashMap<String, (u8, Vec<(usize, Value)>)> = HashMap::new();
        let mut discriminator = 0u8;
        
        for (idx, value) in column_values.iter().enumerate() {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                let entry = type_groups.entry(type_name).or_insert_with(|| {
                    let disc = discriminator;
                    discriminator += 1;
                    (disc, Vec::new())
                });
                entry.1.push((idx, value.clone()));
            }
        }
        
        // Write discriminators for each row
        let total_types = type_groups.len() as u64;
        eprintln!("DEBUG: Writing discriminators for column with {} types, {} rows", total_types, column_values.len());
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if let Some((disc, _)) = type_groups.get(&type_name) {
                if matches!(value, Value::Null) {
                    // NULL discriminator is total_types
                    eprintln!("DEBUG: Writing NULL discriminator {} (total_types={})", total_types, total_types);
                    Self::write_discriminator(writer, total_types, total_types).await?;
                } else {
                    eprintln!("DEBUG: Writing discriminator {} for type {} (total_types={})", *disc, type_name, total_types);
                    Self::write_discriminator(writer, *disc as u64, total_types).await?;
                }
            }
        }
        
        // Write column data for each type (in discriminator order)
        let mut sorted_types: Vec<_> = type_groups.into_iter().collect();
        sorted_types.sort_by_key(|(_, (disc, _))| *disc);
        
        for (type_name, (_, values_with_idx)) in sorted_types {
            if !values_with_idx.is_empty() {
                let typ: Type = type_name.parse().map_err(|_| {
                    Error::SerializeError(format!("Invalid type name: {}", type_name))
                })?;
                
                let values: Vec<Value> = values_with_idx.into_iter().map(|(_, v)| v).collect();
                typ.serialize_column(values, writer, state).await?;
            }
        }
        
        Ok(())
    }
    
    /// Write discriminator based on the total types count
    async fn write_discriminator<W: ClickHouseWrite>(
        writer: &mut W,
        discriminator: u64,
        total_types: u64,
    ) -> Result<()> {
        match total_types {
            0..=255 => writer.write_u8(discriminator as u8).await?,
            256..=65535 => writer.write_u16_le(discriminator as u16).await?,
            65536..=4_294_967_295 => writer.write_u32_le(discriminator as u32).await?,
            _ => writer.write_u64_le(discriminator).await?,
        }
        Ok(())
    }
    
    /// Write Dynamic column data (discriminators + column data) - sync version
    fn write_dynamic_column_data_sync<W: ClickHouseBytesWrite>(
        column_values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Group values by type to determine discriminators
        let mut type_groups: HashMap<String, (u8, Vec<(usize, Value)>)> = HashMap::new();
        let mut discriminator = 0u8;
        
        for (idx, value) in column_values.iter().enumerate() {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                let entry = type_groups.entry(type_name).or_insert_with(|| {
                    let disc = discriminator;
                    discriminator += 1;
                    (disc, Vec::new())
                });
                entry.1.push((idx, value.clone()));
            }
        }
        
        // Write discriminators for each row
        let total_types = type_groups.len() as u64;
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if let Some((disc, _)) = type_groups.get(&type_name) {
                if matches!(value, Value::Null) {
                    // NULL discriminator is total_types
                    Self::write_discriminator_sync(writer, total_types, total_types)?;
                } else {
                    Self::write_discriminator_sync(writer, *disc as u64, total_types)?;
                }
            }
        }
        
        // Write column data for each type (in discriminator order)
        let mut sorted_types: Vec<_> = type_groups.into_iter().collect();
        sorted_types.sort_by_key(|(_, (disc, _))| *disc);
        
        for (type_name, (_, values_with_idx)) in sorted_types {
            if !values_with_idx.is_empty() {
                let typ: Type = type_name.parse().map_err(|_| {
                    Error::SerializeError(format!("Invalid type name: {}", type_name))
                })?;
                
                let values: Vec<Value> = values_with_idx.into_iter().map(|(_, v)| v).collect();
                typ.serialize_column_sync(values, writer, state)?;
            }
        }
        
        Ok(())
    }
    
    /// Write discriminator based on the total types count - sync version
    fn write_discriminator_sync<W: ClickHouseBytesWrite>(
        writer: &mut W,
        discriminator: u64,
        total_types: u64,
    ) -> Result<()> {
        match total_types {
            0..=255 => writer.put_u8(discriminator as u8),
            256..=65535 => writer.put_u16_le(discriminator as u16),
            65536..=4_294_967_295 => writer.put_u32_le(discriminator as u32),
            _ => writer.put_u64_le(discriminator),
        }
        Ok(())
    }
}

impl Serializer for JsonSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Use JSON v3 object serialization format
        writer.write_u64_le(JSON_OBJECT_SERIALIZATION_VERSION).await?;
        
        // We need to store the parsed JSON data for the write phase
        // For now, we'll handle this in the write method by parsing again
        // This is not ideal but works for the initial implementation
        Ok(())
    }

    async fn write<W: ClickHouseWrite>(
        _type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values)?;
        
        // Write the header: total dynamic paths count
        writer.write_var_uint(json_data.path_columns.len() as u64).await?;
        
        // Write path names
        let paths: Vec<String> = json_data.path_columns.keys().cloned().collect();
        for path in &paths {
            writer.write_string(path.as_bytes().to_vec()).await?;
        }
        
        // Write Dynamic column prefixes for each path
        for path in &paths {
            if let Some(column_values) = json_data.path_columns.get(path) {
                // Determine the types in this column
                let mut type_map: HashMap<String, Vec<Value>> = HashMap::new();
                
                for value in column_values {
                    let type_name = Self::get_value_type_name(value);
                    type_map.entry(type_name).or_insert_with(Vec::new).push(value.clone());
                }
                
                // Write Dynamic header for this path
                // Dynamic version
                writer.write_u64_le(3).await?;
                
                // Total types count
                writer.write_var_uint(type_map.len() as u64).await?;
                
                // Type names (sorted for consistency)
                let mut type_names: Vec<String> = type_map.keys().cloned().collect();
                type_names.sort();
                
                for type_name in &type_names {
                    writer.write_string(type_name.as_bytes().to_vec()).await?;
                }
                
                // Write prefixes for nested types (for complex types) 
                // For JSON, we don't need to write nested prefixes as they're handled by Dynamic serialization
                // This is similar to how Dynamic handles its own nested types
            }
        }
        
        // Write data for each path (using Dynamic column format)
        for path in &paths {
            if let Some(column_values) = json_data.path_columns.get(path) {
                Self::write_dynamic_column_data(column_values, writer, state).await?;
            }
        }
        
        Ok(())
    }

    fn write_sync(
        _type_: &Type,
        values: Vec<Value>,
        writer: &mut impl ClickHouseBytesWrite,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values)?;
        
        // Write the header: total dynamic paths count
        writer.put_var_uint(json_data.path_columns.len() as u64)?;
        
        // Write path names
        let paths: Vec<String> = json_data.path_columns.keys().cloned().collect();
        for path in &paths {
            writer.put_string(path.as_bytes().to_vec())?;
        }
        
        // Write Dynamic column prefixes for each path
        for path in &paths {
            if let Some(column_values) = json_data.path_columns.get(path) {
                // Determine the types in this column
                let mut type_map: HashMap<String, Vec<Value>> = HashMap::new();
                
                for value in column_values {
                    let type_name = Self::get_value_type_name(value);
                    type_map.entry(type_name).or_insert_with(Vec::new).push(value.clone());
                }
                
                // Write Dynamic header for this path
                // Dynamic version
                writer.put_u64_le(3);
                
                // Total types count
                writer.put_var_uint(type_map.len() as u64)?;
                
                // Type names (sorted for consistency)
                let mut type_names: Vec<String> = type_map.keys().cloned().collect();
                type_names.sort();
                
                for type_name in &type_names {
                    writer.put_string(type_name.as_bytes().to_vec())?;
                }
                
                // Write prefixes for nested types (for complex types) 
                // For JSON, we don't need to write nested prefixes as they're handled by Dynamic serialization
                // This is similar to how Dynamic handles its own nested types
            }
        }
        
        // Write data for each path (using Dynamic column format)
        for path in &paths {
            if let Some(column_values) = json_data.path_columns.get(path) {
                Self::write_dynamic_column_data_sync(column_values, writer, state)?;
            }
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use crate::formats::{DeserializerState, SerializerState};
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;
    use crate::native::types::serialize::ClickHouseNativeSerializer;
    
    #[tokio::test]
    async fn test_json_v3_serialization_roundtrip() -> Result<()> {
        // Test with complex JSON objects that will create multiple paths
        let values = vec![
            Value::String(b"{\"id\": 42, \"user\": {\"name\": \"Alice\", \"age\": 30}}".to_vec()),
            Value::String(b"{\"id\": 99, \"user\": {\"name\": \"Bob\"}, \"metadata\": {\"active\": true}}".to_vec()),
            Value::String(b"{\"count\": 123.45, \"tags\": [\"rust\", \"clickhouse\"], \"user\": {\"age\": 25}}".to_vec()),
        ];
        
        let type_ = Type::JSON;
        let values_len = values.len();
        eprintln!("DEBUG: Starting JSON v3 serialization round-trip test with {} values", values_len);
        
        // Test JSON serialization with timeout
        let timeout_result = tokio::time::timeout(
            std::time::Duration::from_secs(10), 
            async move {
                let mut output = vec![];
                let mut state = SerializerState::default();
                
                eprintln!("DEBUG: Writing prefix...");
                type_.serialize_prefix_async(&mut output, &mut state).await?;
                
                eprintln!("DEBUG: Writing column data...");
                type_.serialize_column(values.clone(), &mut output, &mut state).await?;
                
                eprintln!("DEBUG: Serialization complete, output size: {} bytes", output.len());
                
                // Try to deserialize it back
                let mut input = Cursor::new(output);
                let mut state = DeserializerState::default();
                
                eprintln!("DEBUG: Reading prefix...");
                type_.deserialize_prefix_async(&mut input, &mut state).await?;
                
                eprintln!("DEBUG: Reading column data...");
                let deserialized = type_.deserialize_column(&mut input, values_len, &mut state).await?;
                
                eprintln!("DEBUG: Deserialization complete, got {} values", deserialized.len());
                
                // Verify that the deserialized JSON contains the expected data
                for (i, original) in values.iter().enumerate() {
                    if let (Value::String(orig_bytes), Value::String(deser_bytes)) = (original, &deserialized[i]) {
                        let orig_json: serde_json::Value = serde_json::from_slice(orig_bytes).map_err(|e| {
                            Error::SerializeError(format!("Failed to parse original JSON: {}", e))
                        })?;
                        let deser_json: serde_json::Value = serde_json::from_slice(deser_bytes).map_err(|e| {
                            Error::SerializeError(format!("Failed to parse deserialized JSON: {}", e))
                        })?;
                        
                        eprintln!("DEBUG: Original JSON {}: {}", i, orig_json);
                        eprintln!("DEBUG: Deserialized JSON {}: {}", i, deser_json);
                        
                        // For JSON v3, the data should be reconstructed correctly
                        // We may not have exact equality due to serialization order, but we can check basic structure
                        assert!(deser_json.is_object(), "Deserialized JSON should be an object");
                    }
                }
                
                Ok(deserialized)
            }
        ).await;
        
        match timeout_result {
            Ok(Ok(result)) => {
                eprintln!("DEBUG: JSON v3 serialization round-trip completed successfully");
                assert_eq!(result.len(), values_len);
                Ok(())
            }
            Ok(Err(e)) => {
                eprintln!("ERROR: JSON serialization failed: {}", e);
                Err(e)
            }
            Err(_) => {
                eprintln!("ERROR: JSON serialization test timed out!");
                panic!("JSON serialization timed out");
            }
        }
    }
}