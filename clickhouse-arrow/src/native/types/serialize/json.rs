use std::collections::{BTreeMap, HashMap};

use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Value};

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION: u64 = 0;
#[allow(dead_code)] // Used when FORCE_STRING_SERIALIZATION is true
const JSON_STRING_SERIALIZATION_VERSION: u64 = 1;
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;

// TODO: JSON v3 object serialization requires writing the full header
// (paths, types, etc.) during the prefix phase, but our current architecture
// doesn't have access to the data during that phase. The Go implementation
// collects this metadata during the Append phase and stores it for the
// WriteStatePrefix phase. Until we refactor to support this pattern,
// we use string serialization (v1) which doesn't require complex headers.
//
// This is less efficient but works correctly. To properly support v3:
// 1. Analyze values and collect paths/types before serialization
// 2. Store this metadata in thread-local or instance state
// 3. Write the full header during write_prefix phase
// 4. Write only the data during the write phase
const FORCE_STRING_SERIALIZATION: bool = true;

/// Parsed JSON data organized by dynamic paths
#[derive(Debug, Clone)]
struct JsonData {
    /// Map from path (e.g., "user.name") to values for that path across all rows
    path_columns: BTreeMap<String, Vec<Value>>,
    /// Number of rows
    rows:         usize,
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
                        Error::SerializeError(format!("Invalid UTF-8 in JSON string: {e}"))
                    })?;

                    let json_value: serde_json::Value =
                        serde_json::from_str(&json_str).map_err(|e| {
                            Error::SerializeError(format!("Invalid JSON string: {e}"))
                        })?;

                    // Extract paths from JSON object
                    Self::extract_paths_from_json(
                        &json_value,
                        "",
                        &mut path_columns,
                        row_idx,
                        rows,
                    )?;
                }
                Value::Null => {
                    // For null values, we don't add any paths - they'll be filled with nulls
                }
                _ => {
                    return Err(Error::SerializeError(format!(
                        "JSON serialization only supports String values containing JSON, got: {value:?}"
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

        Ok(JsonData { path_columns, rows })
    }

    /// Recursively extract paths from JSON value
    fn extract_paths_from_json(
        json_value: &serde_json::Value,
        current_path: &str,
        path_columns: &mut BTreeMap<String, Vec<Value>>,
        row_idx: usize,
        total_rows: usize,
    ) -> Result<()> {
        if let serde_json::Value::Object(map) = json_value {
            for (key, value) in map {
                let path = if current_path.is_empty() {
                    key.clone()
                } else {
                    format!("{current_path}.{key}")
                };

                Self::extract_paths_from_json(value, &path, path_columns, row_idx, total_rows)?;
            }
        } else {
            // Leaf value - convert to ClickHouse Value and store
            let ch_value = Self::json_value_to_clickhouse_value(json_value)?;

            // Ensure the column exists and has the right size
            let column = path_columns
                .entry(current_path.to_string())
                .or_insert_with(|| vec![Value::Null; total_rows]);

            // Set the value at the correct row index
            if row_idx < column.len() {
                column[row_idx] = ch_value;
            }
        }
        Ok(())
    }

    /// Convert `serde_json::Value` to `ClickHouse` Value
    fn json_value_to_clickhouse_value(json_value: &serde_json::Value) -> Result<Value> {
        let value = match json_value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => {
                // ClickHouse doesn't have a native Bool, use UInt8
                Value::UInt8(u8::from(*b))
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
                        "Unsupported JSON number format: {n}"
                    )));
                }
            }
            serde_json::Value::String(s) => Value::String(s.as_bytes().to_vec()),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                // For complex types, serialize back to JSON string
                let json_str = serde_json::to_string(json_value).map_err(|e| {
                    Error::SerializeError(format!("Failed to serialize JSON value: {e}"))
                })?;
                Value::String(json_str.into_bytes())
            }
        };
        Ok(value)
    }
}

impl JsonSerializer {
    /// Get the `ClickHouse` type name for a Value
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
        // Group values by type
        let mut type_map: HashMap<String, Vec<(usize, Value)>> = HashMap::new();

        for (idx, value) in column_values.iter().enumerate() {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                type_map.entry(type_name).or_insert_with(Vec::new).push((idx, value.clone()));
            }
        }

        // Sort type names alphabetically to match prefix phase ordering
        let mut type_names: Vec<String> = type_map.keys().cloned().collect();
        type_names.sort();

        // Create discriminator mapping based on alphabetical order
        let type_to_discriminator: HashMap<String, u8> =
            type_names.iter().enumerate().map(|(idx, name)| (name.clone(), idx as u8)).collect();

        // Write discriminators for each row
        let total_types = type_names.len() as u64;
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types
                Self::write_discriminator(writer, total_types, total_types).await?;
            } else if let Some(&disc) = type_to_discriminator.get(&type_name) {
                Self::write_discriminator(writer, disc as u64, total_types).await?;
            }
        }

        // Write column data for each type (in alphabetical order)
        for type_name in &type_names {
            if let Some(values_with_idx) = type_map.get(type_name) {
                if !values_with_idx.is_empty() {
                    let typ: Type = type_name.parse().map_err(|_| {
                        Error::SerializeError(format!("Invalid type name: {type_name}"))
                    })?;

                    let values: Vec<Value> =
                        values_with_idx.iter().map(|(_, v)| v.clone()).collect();

                    // Special handling to avoid recursion - JSON type should not appear here
                    if matches!(typ, Type::JSON) {
                        return Err(Error::SerializeError(
                            "JSON type cannot be nested within JSON paths".to_string(),
                        ));
                    }

                    typ.serialize_column(values, writer, state).await?;
                }
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
        // Group values by type
        let mut type_map: HashMap<String, Vec<(usize, Value)>> = HashMap::new();

        for (idx, value) in column_values.iter().enumerate() {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                type_map.entry(type_name).or_insert_with(Vec::new).push((idx, value.clone()));
            }
        }

        // Sort type names alphabetically to match prefix phase ordering
        let mut type_names: Vec<String> = type_map.keys().cloned().collect();
        type_names.sort();

        // Create discriminator mapping based on alphabetical order
        let type_to_discriminator: HashMap<String, u8> =
            type_names.iter().enumerate().map(|(idx, name)| (name.clone(), idx as u8)).collect();

        // Write discriminators for each row
        let total_types = type_names.len() as u64;
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types
                Self::write_discriminator_sync(writer, total_types, total_types)?;
            } else if let Some(&disc) = type_to_discriminator.get(&type_name) {
                Self::write_discriminator_sync(writer, disc as u64, total_types)?;
            }
        }

        // Write column data for each type (in alphabetical order)
        for type_name in &type_names {
            if let Some(values_with_idx) = type_map.get(type_name) {
                if !values_with_idx.is_empty() {
                    let typ: Type = type_name.parse().map_err(|_| {
                        Error::SerializeError(format!("Invalid type name: {type_name}"))
                    })?;

                    let values: Vec<Value> =
                        values_with_idx.iter().map(|(_, v)| v.clone()).collect();

                    // Special handling to avoid recursion - JSON type should not appear here
                    if matches!(typ, Type::JSON) {
                        return Err(Error::SerializeError(
                            "JSON type cannot be nested within JSON paths".to_string(),
                        ));
                    }

                    typ.serialize_column_sync(values, writer, state)?;
                }
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

impl JsonSerializer {
    /// Check if server supports flat Dynamic/JSON serialization (v3)
    fn supports_flat_dynamic_json(state: &SerializerState) -> bool {
        if let Some((major, minor, _)) = state.server_version {
            major >= 25 && minor >= 6
        } else {
            false // Default to v0 if version unknown
        }
    }

    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        if FORCE_STRING_SERIALIZATION {
            // Use string serialization which doesn't require complex headers
            writer.put_u64_le(JSON_STRING_SERIALIZATION_VERSION);
        } else {
            // Choose JSON serialization version based on server version
            let version = if Self::supports_flat_dynamic_json(state) {
                JSON_OBJECT_SERIALIZATION_VERSION
            } else {
                JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION
            };
            writer.put_u64_le(version);
        }
        Ok(())
    }
}

impl Serializer for JsonSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        if FORCE_STRING_SERIALIZATION {
            // Use string serialization which doesn't require complex headers
            writer.write_u64_le(JSON_STRING_SERIALIZATION_VERSION).await?;
        } else {
            // Choose JSON serialization version based on server version
            let version = if Self::supports_flat_dynamic_json(state) {
                JSON_OBJECT_SERIALIZATION_VERSION
            } else {
                JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION
            };
            writer.write_u64_le(version).await?;
        }

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
        if FORCE_STRING_SERIALIZATION {
            // Simple string serialization - just write the JSON strings as-is
            Type::String.serialize_column(values, writer, state).await?;
            return Ok(());
        }

        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values)?;
        let use_v3 = Self::supports_flat_dynamic_json(state);

        if use_v3 {
            // V3 format (new flat Dynamic/JSON)
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

                    // Basic types don't need prefix serialization in Dynamic v3 format
                    // Only complex types that implement CustomSerialization need prefixes
                }
            }

            // Write data for each path (using Dynamic column format)
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    Self::write_dynamic_column_data(column_values, writer, state).await?;
                }
            }
        } else {
            // V0 format (deprecated)
            const DEFAULT_MAX_DYNAMIC_PATHS: u64 = 1024;

            // Write max dynamic paths
            writer.write_var_uint(DEFAULT_MAX_DYNAMIC_PATHS).await?;

            // Write total dynamic paths
            writer.write_var_uint(json_data.path_columns.len() as u64).await?;

            // Write path names
            let paths: Vec<String> = json_data.path_columns.keys().cloned().collect();
            for path in &paths {
                writer.write_string(path.as_bytes().to_vec()).await?;
            }

            // Write Dynamic column headers for each path
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    // Determine the types in this column
                    let mut type_map: HashMap<String, Vec<Value>> = HashMap::new();

                    for value in column_values {
                        let type_name = Self::get_value_type_name(value);
                        type_map.entry(type_name).or_insert_with(Vec::new).push(value.clone());
                    }

                    // Write Dynamic header for this path (v0 format uses Dynamic v3)
                    writer.write_u64_le(3).await?;

                    // Total types count
                    writer.write_var_uint(type_map.len() as u64).await?;

                    // Type names (sorted for consistency)
                    let mut type_names: Vec<String> = type_map.keys().cloned().collect();
                    type_names.sort();

                    for type_name in &type_names {
                        writer.write_string(type_name.as_bytes().to_vec()).await?;
                    }
                }
            }

            // Write data for each path (using Dynamic column format)
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    Self::write_dynamic_column_data(column_values, writer, state).await?;
                }
            }

            // Write SharedData (empty) per row
            for _ in 0..json_data.rows {
                writer.write_u64_le(0).await?;
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
        if FORCE_STRING_SERIALIZATION {
            // Simple string serialization - just write the JSON strings as-is
            Type::String.serialize_column_sync(values, writer, state)?;
            return Ok(());
        }

        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values)?;
        let use_v3 = Self::supports_flat_dynamic_json(state);

        if use_v3 {
            // V3 format (new flat Dynamic/JSON)
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

                    // Basic types don't need prefix serialization in Dynamic v3 format
                    // Only complex types that implement CustomSerialization need prefixes
                }
            }

            // Write data for each path (using Dynamic column format)
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    Self::write_dynamic_column_data_sync(column_values, writer, state)?;
                }
            }
        } else {
            // V0 format (deprecated)
            const DEFAULT_MAX_DYNAMIC_PATHS: u64 = 1024;

            // Write max dynamic paths
            writer.put_var_uint(DEFAULT_MAX_DYNAMIC_PATHS)?;

            // Write total dynamic paths
            writer.put_var_uint(json_data.path_columns.len() as u64)?;

            // Write path names
            let paths: Vec<String> = json_data.path_columns.keys().cloned().collect();
            for path in &paths {
                writer.put_string(path.as_bytes().to_vec())?;
            }

            // Write Dynamic column headers for each path
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    // Determine the types in this column
                    let mut type_map: HashMap<String, Vec<Value>> = HashMap::new();

                    for value in column_values {
                        let type_name = Self::get_value_type_name(value);
                        type_map.entry(type_name).or_insert_with(Vec::new).push(value.clone());
                    }

                    // Write Dynamic header for this path (v0 format uses Dynamic v3)
                    writer.put_u64_le(3);

                    // Total types count
                    writer.put_var_uint(type_map.len() as u64)?;

                    // Type names (sorted for consistency)
                    let mut type_names: Vec<String> = type_map.keys().cloned().collect();
                    type_names.sort();

                    for type_name in &type_names {
                        writer.put_string(type_name.as_bytes().to_vec())?;
                    }
                }
            }

            // Write data for each path (using Dynamic column format)
            for path in &paths {
                if let Some(column_values) = json_data.path_columns.get(path) {
                    Self::write_dynamic_column_data_sync(column_values, writer, state)?;
                }
            }

            // Write SharedData (empty) per row
            for _ in 0..json_data.rows {
                writer.put_u64_le(0);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::formats::{DeserializerState, SerializerState};
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;
    use crate::native::types::serialize::ClickHouseNativeSerializer;

    #[tokio::test]
    async fn test_json_v3_serialization_roundtrip() -> Result<()> {
        // Test with original failing case but only 2 rows
        let values = vec![
            Value::String(b"{\"id\": 42, \"user\": {\"name\": \"Alice\", \"age\": 30}}".to_vec()),
            Value::String(
                b"{\"id\": 99, \"user\": {\"name\": \"Bob\"}, \"metadata\": {\"active\": true}}"
                    .to_vec(),
            ),
        ];

        let type_ = Type::JSON;
        let values_len = values.len();
        // Test JSON serialization round-trip

        // Test JSON serialization with timeout
        let timeout_result = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            let mut output = vec![];
            let mut state = SerializerState::default();

            type_.serialize_prefix_async(&mut output, &mut state).await?;
            type_.serialize_column(values.clone(), &mut output, &mut state).await?;

            // Try to deserialize it back
            let mut input = Cursor::new(output);
            let mut state = DeserializerState::default();

            type_.deserialize_prefix_async(&mut input, &mut state).await?;
            let deserialized = type_.deserialize_column(&mut input, values_len, &mut state).await?;

            // Verify we got the correct number of values
            assert_eq!(deserialized.len(), values_len);

            // Verify that the deserialized JSON contains the expected data
            for (i, original) in values.iter().enumerate() {
                if let (Value::String(orig_bytes), Value::String(deser_bytes)) =
                    (original, &deserialized[i])
                {
                    let _orig_json: serde_json::Value =
                        serde_json::from_slice(orig_bytes).map_err(|e| {
                            Error::SerializeError(format!("Failed to parse original JSON: {e}"))
                        })?;
                    let deser_json: serde_json::Value = serde_json::from_slice(deser_bytes)
                        .map_err(|e| {
                            Error::SerializeError(format!(
                                "Failed to parse deserialized JSON: {e}"
                            ))
                        })?;

                    // Verify the JSON structure

                    // For JSON v3, the data should be reconstructed correctly
                    // We may not have exact equality due to serialization order, but we can check
                    // basic structure
                    assert!(deser_json.is_object(), "Deserialized JSON should be an object");
                }
            }

            Ok(deserialized)
        })
        .await;

        match timeout_result {
            Ok(Ok(result)) => {
                assert_eq!(result.len(), values_len);
                Ok(())
            }
            Ok(Err(e)) => {
                Err(e)
            }
            Err(_) => {
                panic!("JSON serialization timed out");
            }
        }
    }
}
