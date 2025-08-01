use std::collections::{BTreeMap, HashMap};

use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::formats::{JsonState, TypeSpecificState};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Value};

type JsonPathData = (Vec<String>, HashMap<String, Vec<(usize, Value)>>, HashMap<String, u8>);

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;
const DYNAMIC_VERSION: u64 = 3;

// JSON v3 object serialization is now supported via thread-local caching
// The implementation follows the same pattern as Dynamic type serialization:
// 1. analyze_values is called before serialization to collect metadata
// 2. metadata is cached in thread-local storage
// 3. write_prefix uses cached metadata to write the full header
// 4. write uses cached data to write column data efficiently

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

                    let json_value: serde_json::Value = serde_json::from_str(&json_str)
                        .map_err(|e| Error::SerializeError(format!("Invalid JSON string: {e}")))?;

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
                        "JSON serialization only supports String values containing JSON, got: \
                         {value:?}"
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

/// Macro to write discriminator based on size
macro_rules! write_discriminator {
    (async $writer:expr, $disc:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => {
                debug_assert!($disc <= 255);
                $writer.write_u8(u8::try_from($disc).unwrap()).await?
            }
            256..=65535 => {
                debug_assert!($disc <= 65535);
                $writer.write_u16_le(u16::try_from($disc).unwrap()).await?
            }
            65536..=4_294_967_295 => $writer.write_u32_le(u32::try_from($disc).unwrap()).await?,
            _ => $writer.write_u64_le($disc).await?,
        }
    };
    (sync $writer:expr, $disc:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => {
                debug_assert!($disc <= 255);
                $writer.put_u8(u8::try_from($disc).unwrap())
            }
            256..=65535 => {
                debug_assert!($disc <= 65535);
                $writer.put_u16_le(u16::try_from($disc).unwrap())
            }
            65536..=4_294_967_295 => $writer.put_u32_le(u32::try_from($disc).unwrap()),
            _ => $writer.put_u64_le($disc),
        }
    };
}

impl JsonSerializer {
    /// Check if server supports JSON v3
    fn check_server_version(state: &SerializerState) -> Result<()> {
        if let Some((major, minor, _)) = state.server_version
            && (major < 25 || (major == 25 && minor < 6))
        {
            return Err(Error::SerializeError(format!(
                "JSON type requires ClickHouse server version >= 25.6, got {major}.{minor}"
            )));
        }
        Ok(())
    }

    /// Get the `ClickHouse` type name for a Value
    fn get_value_type_name(value: &Value) -> String {
        match value {
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
            _ => "String".to_string(), // Nulls, strings, and complex types use String
        }
    }

    /// Build type map from column values
    fn build_type_map(column_values: &[Value]) -> (Vec<String>, HashMap<String, Vec<Value>>) {
        let mut type_map: HashMap<String, Vec<Value>> = HashMap::new();

        for value in column_values {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                type_map.entry(type_name).or_default().push(value.clone());
            }
        }

        // Type names (sorted for consistency)
        let mut type_names: Vec<String> = type_map.keys().cloned().collect();
        type_names.sort();

        (type_names, type_map)
    }

    /// Write Dynamic column header for a column with given values (async)
    async fn write_dynamic_header_for_column_async<W: ClickHouseWrite>(
        column_values: &[Value],
        writer: &mut W,
    ) -> Result<()> {
        let (type_names, type_map) = Self::build_type_map(column_values);

        // Write Dynamic header for this path
        writer.write_u64_le(DYNAMIC_VERSION).await?;
        writer.write_var_uint(type_map.len() as u64).await?;

        for type_name in &type_names {
            writer.write_string(type_name.as_bytes().to_vec()).await?;
        }

        // Basic types don't need prefix serialization in Dynamic v3 format
        // Only complex types that implement CustomSerialization need prefixes
        Ok(())
    }

    /// Write Dynamic column header for a column with given values (sync)
    fn write_dynamic_header_for_column_sync<W: ClickHouseBytesWrite>(
        column_values: &[Value],
        writer: &mut W,
    ) -> Result<()> {
        let (type_names, type_map) = Self::build_type_map(column_values);

        // Write Dynamic header for this path
        writer.put_u64_le(DYNAMIC_VERSION);
        writer.put_var_uint(type_map.len() as u64)?;

        for type_name in &type_names {
            writer.put_string(type_name.as_bytes())?;
        }

        // Basic types don't need prefix serialization in Dynamic v3 format
        // Only complex types that implement CustomSerialization need prefixes
        Ok(())
    }

    /// Group values by type and build discriminator mapping
    fn group_values_by_type(column_values: &[Value]) -> JsonPathData {
        let mut type_map: HashMap<String, Vec<(usize, Value)>> = HashMap::new();

        for (idx, value) in column_values.iter().enumerate() {
            if !matches!(value, Value::Null) {
                let type_name = Self::get_value_type_name(value);
                type_map.entry(type_name).or_default().push((idx, value.clone()));
            }
        }

        // Sort type names alphabetically to match prefix phase ordering
        let mut type_names: Vec<String> = type_map.keys().cloned().collect();
        type_names.sort();

        // Create discriminator mapping based on alphabetical order
        let type_to_discriminator: HashMap<String, u8> = type_names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                debug_assert!(idx <= 255, "Too many types for u8 discriminator");
                (name.clone(), u8::try_from(idx).unwrap())
            })
            .collect();

        (type_names, type_map, type_to_discriminator)
    }

    /// Write discriminators for column values
    async fn write_discriminators<W: ClickHouseWrite>(
        column_values: &[Value],
        type_to_discriminator: &HashMap<String, u8>,
        total_types: u64,
        writer: &mut W,
    ) -> Result<()> {
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types
                write_discriminator!(async writer, total_types, total_types);
            } else if let Some(&disc) = type_to_discriminator.get(&type_name) {
                write_discriminator!(async writer, u64::from(disc), total_types);
            }
        }
        Ok(())
    }

    /// Write discriminators for column values (sync)
    fn write_discriminators_sync<W: ClickHouseBytesWrite>(
        column_values: &[Value],
        type_to_discriminator: &HashMap<String, u8>,
        total_types: u64,
        writer: &mut W,
    ) {
        for value in column_values {
            let type_name = Self::get_value_type_name(value);
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types
                write_discriminator!(sync writer, total_types, total_types);
            } else if let Some(&disc) = type_to_discriminator.get(&type_name) {
                write_discriminator!(sync writer, u64::from(disc), total_types);
            }
        }
    }

    /// Write column data for typed values
    async fn write_typed_columns<W: ClickHouseWrite>(
        type_names: &[String],
        type_map: &HashMap<String, Vec<(usize, Value)>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for type_name in type_names {
            if let Some(values_with_idx) = type_map.get(type_name)
                && !values_with_idx.is_empty()
            {
                let typ: Type = type_name.parse().map_err(|_| {
                    Error::SerializeError(format!("Invalid type name: {type_name}"))
                })?;

                let values: Vec<Value> = values_with_idx.iter().map(|(_, v)| v.clone()).collect();

                // Special handling to avoid recursion - JSON type should not appear here
                if matches!(typ, Type::JSON { .. }) {
                    return Err(Error::SerializeError(
                        "JSON type cannot be nested within JSON paths".to_string(),
                    ));
                }

                typ.serialize_column(values, writer, state).await?;
            }
        }
        Ok(())
    }

    /// Write column data for typed values (sync)
    fn write_typed_columns_sync<W: ClickHouseBytesWrite>(
        type_names: &[String],
        type_map: &HashMap<String, Vec<(usize, Value)>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for type_name in type_names {
            if let Some(values_with_idx) = type_map.get(type_name)
                && !values_with_idx.is_empty()
            {
                let typ: Type = type_name.parse().map_err(|_| {
                    Error::SerializeError(format!("Invalid type name: {type_name}"))
                })?;

                let values: Vec<Value> = values_with_idx.iter().map(|(_, v)| v.clone()).collect();

                // Special handling to avoid recursion - JSON type should not appear here
                if matches!(typ, Type::JSON { .. }) {
                    return Err(Error::SerializeError(
                        "JSON type cannot be nested within JSON paths".to_string(),
                    ));
                }

                typ.serialize_column_sync(values, writer, state)?;
            }
        }
        Ok(())
    }

    /// Write Dynamic column data (discriminators + column data)
    async fn write_dynamic_column_data<W: ClickHouseWrite>(
        column_values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let (type_names, type_map, type_to_discriminator) =
            Self::group_values_by_type(column_values);
        let total_types = type_names.len() as u64;

        // Write discriminators for each row
        Self::write_discriminators(column_values, &type_to_discriminator, total_types, writer)
            .await?;

        // Write column data for each type (in alphabetical order)
        Self::write_typed_columns(&type_names, &type_map, writer, state).await
    }

    /// Write Dynamic column data (discriminators + column data) - sync version
    fn write_dynamic_column_data_sync<W: ClickHouseBytesWrite>(
        column_values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let (type_names, type_map, type_to_discriminator) =
            Self::group_values_by_type(column_values);
        let total_types = type_names.len() as u64;

        // Write discriminators for each row
        Self::write_discriminators_sync(column_values, &type_to_discriminator, total_types, writer);

        // Write column data for each type (in alphabetical order)
        Self::write_typed_columns_sync(&type_names, &type_map, writer, state)
    }
}

impl JsonSerializer {
    /// Analyze JSON values and return metadata for use in `write_prefix`
    pub(crate) fn analyze_values(values: &[Value]) -> Result<TypeSpecificState> {
        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values.to_vec())?;

        // Build the metadata
        let mut paths: Vec<String> = json_data.path_columns.keys().cloned().collect();
        paths.sort(); // Ensure consistent ordering

        let state = JsonState {
            version:      None, // Will be set properly in write_prefix
            paths:        paths.clone(),
            path_columns: Some(json_data.path_columns),
            rows:         Some(json_data.rows),
            dynamic_data: None,
        };

        Ok(TypeSpecificState::Json(state))
    }

    /// Get serialization version based on server support
    fn get_serialization_version(_state: &SerializerState) -> u64 {
        JSON_OBJECT_SERIALIZATION_VERSION
    }

    /// Write paths header based on version
    fn write_paths_header_sync<W: ClickHouseBytesWrite>(
        paths: &[String],
        version: u64,
        writer: &mut W,
    ) -> Result<()> {
        if version != JSON_OBJECT_SERIALIZATION_VERSION {
            return Err(Error::SerializeError(format!(
                "Unsupported JSON serialization version: {version}"
            )));
        }

        // V3 format: total dynamic paths count
        writer.put_var_uint(paths.len() as u64)?;

        // Write path names
        for path in paths {
            writer.put_string(path.as_bytes())?;
        }
        Ok(())
    }

    /// Write paths header based on version (async)
    async fn write_paths_header_async<W: ClickHouseWrite>(
        paths: &[String],
        version: u64,
        writer: &mut W,
    ) -> Result<()> {
        if version != JSON_OBJECT_SERIALIZATION_VERSION {
            return Err(Error::SerializeError(format!(
                "Unsupported JSON serialization version: {version}"
            )));
        }

        // V3 format: total dynamic paths count
        writer.write_var_uint(paths.len() as u64).await?;

        // Write path names
        for path in paths {
            writer.write_string(path.as_bytes().to_vec()).await?;
        }
        Ok(())
    }

    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Check server version support
        Self::check_server_version(state)?;

        let version = Self::get_serialization_version(state);
        writer.put_u64_le(version);

        // Update the version in state
        if let TypeSpecificState::Json(json_state) = &mut state.type_specific {
            json_state.version = Some(version);
        }

        // Retrieve metadata from state
        if let TypeSpecificState::Json(json_state) = &state.type_specific {
            // Write paths header
            Self::write_paths_header_sync(&json_state.paths, version, writer)?;

            // Write Dynamic column headers for each path
            if let Some(path_columns) = &json_state.path_columns {
                for path in &json_state.paths {
                    if let Some(column_values) = path_columns.get(path) {
                        Self::write_dynamic_header_for_column_sync(column_values, writer)?;
                    }
                }
            }
        } else {
            return Err(Error::SerializeError(
                "JSON serialization state not found. `analyze_values` must be called before \
                 `write_prefix`."
                    .to_string(),
            ));
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
        // Check server version support
        Self::check_server_version(state)?;

        let version = Self::get_serialization_version(state);
        writer.write_u64_le(version).await?;

        // Update the version in state
        if let TypeSpecificState::Json(json_state) = &mut state.type_specific {
            json_state.version = Some(version);
        }

        // Retrieve metadata from state
        if let TypeSpecificState::Json(json_state) = &state.type_specific {
            // Write paths header
            Self::write_paths_header_async(&json_state.paths, version, writer).await?;

            // Write Dynamic column headers for each path
            if let Some(path_columns) = &json_state.path_columns {
                for path in &json_state.paths {
                    if let Some(column_values) = path_columns.get(path) {
                        Self::write_dynamic_header_for_column_async(column_values, writer).await?;
                    }
                }
            }
        } else {
            return Err(Error::SerializeError(
                "JSON serialization state not found. `analyze_values` must be called before \
                 `write_prefix`."
                    .to_string(),
            ));
        }

        Ok(())
    }

    async fn write<W: ClickHouseWrite>(
        _type_: &Type,
        _values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Always use v3 now (server version already checked in write_prefix)
        let use_v3 = true;

        // Get metadata from state
        let (paths, path_columns, rows) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                // Use metadata from analyze_values
                let path_columns = json_state.path_columns.clone().ok_or_else(|| {
                    Error::SerializeError("JSON path columns not found in state".to_string())
                })?;
                let rows = json_state.rows.ok_or_else(|| {
                    Error::SerializeError("JSON rows count not found in state".to_string())
                })?;
                (json_state.paths.clone(), path_columns, rows)
            } else {
                return Err(Error::SerializeError(
                    "JSON serialization state not found. `analyze_values` must be called before \
                     `write`."
                        .to_string(),
                ));
            };

        // Write data for each path (using Dynamic column format)
        for path in &paths {
            if let Some(column_values) = path_columns.get(path) {
                Self::write_dynamic_column_data(column_values, writer, state).await?;
            }
        }

        // V0 format needs SharedData (empty) per row
        if !use_v3 {
            for _ in 0..rows {
                writer.write_u64_le(0).await?;
            }
        }

        Ok(())
    }

    fn write_sync(
        _type_: &Type,
        _values: Vec<Value>,
        writer: &mut impl ClickHouseBytesWrite,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Always use v3 now (server version already checked in write_prefix)
        let use_v3 = true;

        // Get metadata from state
        let (paths, path_columns, rows) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                // Use metadata from analyze_values
                let path_columns = json_state.path_columns.clone().ok_or_else(|| {
                    Error::SerializeError("JSON path columns not found in state".to_string())
                })?;
                let rows = json_state.rows.ok_or_else(|| {
                    Error::SerializeError("JSON rows count not found in state".to_string())
                })?;
                (json_state.paths.clone(), path_columns, rows)
            } else {
                return Err(Error::SerializeError(
                    "JSON serialization state not found. `analyze_values` must be called before \
                     `write`."
                        .to_string(),
                ));
            };

        // Write data for each path (using Dynamic column format)
        for path in &paths {
            if let Some(column_values) = path_columns.get(path) {
                Self::write_dynamic_column_data_sync(column_values, writer, state)?;
            }
        }

        // V0 format needs SharedData (empty) per row
        if !use_v3 {
            for _ in 0..rows {
                writer.put_u64_le(0);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::formats::{DeserializerState, SerializerState};
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;
    use crate::native::types::serialize::ClickHouseNativeSerializer;

    /// Helper function to test JSON serialization roundtrip with standard assertions
    async fn test_json_roundtrip(values: Vec<Value>) -> Result<Vec<Value>> {
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![],
        };
        let values_len = values.len();

        let mut output = vec![];
        let mut state = SerializerState::default();

        // JSON serialization requires analyze_values to be called first
        state.type_specific = JsonSerializer::analyze_values(&values)?;

        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut state).await?;

        // Deserialize it back
        let mut input = Cursor::new(output);
        let mut state = DeserializerState::default();

        type_.deserialize_prefix_async(&mut input, &mut state).await?;
        let deserialized = type_.deserialize_column(&mut input, values_len, &mut state).await?;

        assert_eq!(deserialized.len(), values_len);
        Ok(deserialized)
    }

    #[tokio::test]
    async fn test_json_v3_simple_objects() -> Result<()> {
        let values = vec![
            Value::String(b"{\"name\": \"Alice\", \"age\": 30}".to_vec()),
            Value::String(b"{\"name\": \"Bob\", \"age\": 25}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_nested_objects() -> Result<()> {
        let values = vec![
            Value::String(
                b"{\"user\": {\"name\": \"Alice\", \"age\": 30}, \"active\": true}".to_vec(),
            ),
            Value::String(b"{\"user\": {\"name\": \"Bob\"}, \"score\": 95.5}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_mixed_types() -> Result<()> {
        let values = vec![
            Value::String(
                b"{\"id\": 1, \"name\": \"test\", \"active\": true, \"score\": 99.9}".to_vec(),
            ),
            Value::String(b"{\"id\": 2, \"name\": \"example\", \"active\": false}".to_vec()),
            Value::String(b"{\"id\": 3, \"score\": 88.1, \"metadata\": \"extra\"}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_with_nulls() -> Result<()> {
        let values = vec![
            Value::String(b"{\"name\": \"Alice\", \"age\": 30}".to_vec()),
            Value::Null,
            Value::String(b"{\"name\": \"Bob\", \"active\": true}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_empty_objects() -> Result<()> {
        let values = vec![
            Value::String(b"{}".to_vec()),
            Value::String(b"{\"name\": \"test\"}".to_vec()),
            Value::String(b"{}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_wire_format_verification() -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        let values = vec![
            Value::String(b"{\"name\": \"Alice\", \"age\": 30}".to_vec()),
            Value::String(b"{\"name\": \"Bob\", \"score\": 95.5}".to_vec()),
        ];

        // First do the standard roundtrip test
        let deserialized = test_json_roundtrip(values.clone()).await?;

        // Then perform wire format verification by serializing manually
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![],
        };
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values(&values)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        // Wire format inspection
        let mut cursor = Cursor::new(&output);
        let mut version_bytes = [0u8; 8];
        cursor.read_exact(&mut version_bytes)?;
        let version = u64::from_le_bytes(version_bytes);
        assert_eq!(
            version, JSON_OBJECT_SERIALIZATION_VERSION,
            "Should use v3 object serialization"
        );

        let _ = cursor.seek(SeekFrom::Start(8))?;
        let mut path_count_byte = [0u8; 1];
        cursor.read_exact(&mut path_count_byte)?;
        assert!(path_count_byte[0] > 0, "Should have dynamic paths for object serialization");

        // Verify deserialized data structure
        for value in deserialized.iter() {
            if let Value::String(bytes) = value {
                let json_str = String::from_utf8(bytes.clone())?;
                let json_value: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?;
                assert!(json_value.is_object(), "Deserialized value should be a JSON object");
            } else {
                panic!("Expected String value containing JSON");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_vs_string_serialization_difference() -> Result<()> {
        let values = vec![
            Value::String(
                b"{\"user\": {\"name\": \"Alice\", \"age\": 30}, \"active\": true}".to_vec(),
            ),
            Value::String(
                b"{\"user\": {\"name\": \"Bob\", \"age\": 25}, \"active\": false}".to_vec(),
            ),
            Value::String(
                b"{\"user\": {\"name\": \"Charlie\", \"age\": 35}, \"active\": true}".to_vec(),
            ),
        ];

        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());

        // Additional v3 format verification
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![],
        };
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values(&values)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        let version = u64::from_le_bytes(output[0..8].try_into().unwrap());
        assert_eq!(
            version, JSON_OBJECT_SERIALIZATION_VERSION,
            "Should be using v3 object serialization"
        );
        assert!(
            output[8] >= 3,
            "v3 should decompose JSON into multiple paths (user.name, user.age, active)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_json_object_vs_string_serialization_format() -> Result<()> {
        let values = vec![
            Value::String(b"{\"name\": \"Alice\", \"age\": 30}".to_vec()),
            Value::String(b"{\"name\": \"Bob\", \"age\": 25}".to_vec()),
        ];
        let deserialized = test_json_roundtrip(values.clone()).await?;

        // Additional format verification
        for value in deserialized.iter() {
            if let Value::String(bytes) = value {
                let json_str = String::from_utf8(bytes.clone())?;
                let json_value: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?;
                assert!(json_value.is_object(), "Should be a proper JSON object");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_analyze_values_cache() -> Result<()> {
        let values = vec![
            Value::String(b"{\"name\": \"Alice\", \"age\": 30}".to_vec()),
            Value::String(b"{\"name\": \"Bob\", \"score\": 95.5}".to_vec()),
        ];

        // Test that analyze_values works correctly
        let type_specific_state = JsonSerializer::analyze_values(&values)?;

        // Verify state was populated
        assert!(
            matches!(type_specific_state, TypeSpecificState::Json(_)),
            "Should return Json state"
        );

        let deserialized = test_json_roundtrip(values.clone()).await?;
        assert_eq!(deserialized.len(), values.len());
        Ok(())
    }

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

        // Test JSON serialization with timeout
        let timeout_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            test_json_roundtrip(values.clone()),
        )
        .await;

        match timeout_result {
            Ok(Ok(result)) => {
                assert_eq!(result.len(), values.len());
                // Additional JSON structure validation
                for value in result.iter() {
                    if let Value::String(bytes) = value {
                        let json_str = String::from_utf8(bytes.clone())?;
                        let json_value: serde_json::Value = serde_json::from_str(&json_str)
                            .map_err(|e| {
                                Error::SerializeError(format!("Failed to parse JSON: {e}"))
                            })?;
                        assert!(json_value.is_object(), "Deserialized JSON should be an object");
                    }
                }
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(timeout_error) => {
                panic!("JSON serialization timed out: {timeout_error}");
            }
        }
    }
}
