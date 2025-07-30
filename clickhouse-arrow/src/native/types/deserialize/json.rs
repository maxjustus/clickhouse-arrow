use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::formats::{JsonState as JsonStateData, TypeSpecificState};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::{Error, Result};

// JSON serialization versions
const JSON_STRING_VERSION: u64 = 1;
const JSON_OBJECT_VERSION_2: u64 = 2;
const JSON_OBJECT_VERSION_3: u64 = 3;

/// Macro to read discriminator based on size
macro_rules! read_discriminator {
    (async $reader:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => u64::from($reader.read_u8().await?),
            256..=65535 => u64::from($reader.read_u16_le().await?),
            65536..=4_294_967_295 => u64::from($reader.read_u32_le().await?),
            _ => $reader.read_u64_le().await?,
        }
    };
    (sync $reader:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => u64::from($reader.get_u8()),
            256..=65535 => u64::from($reader.get_u16_le()),
            65536..=4_294_967_295 => u64::from($reader.get_u32_le()),
            _ => $reader.get_u64_le(),
        }
    };
}

pub(crate) struct JsonDeserializer;

impl JsonDeserializer {
    /// Set a value at a nested path in a JSON object map
    fn set_nested_value(
        object: &mut serde_json::Map<String, serde_json::Value>,
        path: &str,
        value: Value,
    ) -> Result<()> {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() {
            return Err(Error::DeserializeError("Empty path".to_string()));
        }

        // Navigate to nested location
        let mut current = object;
        for part in &parts[..parts.len() - 1] {
            let entry = current
                .entry((*part).to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

            match entry {
                serde_json::Value::Object(map) => current = map,
                _ => {
                    return Err(Error::DeserializeError(format!(
                        "Path conflict: '{part}' is not an object"
                    )));
                }
            }
        }

        // Set final value
        let json_value = Self::value_to_json(value)?;
        let old = current.insert(parts[parts.len() - 1].to_string(), json_value);
        debug_assert!(old.is_none() || matches!(old, Some(serde_json::Value::Null)));
        Ok(())
    }

    /// Convert `ClickHouse` Value to JSON
    fn value_to_json(value: Value) -> Result<serde_json::Value> {
        use serde_json::{Number, Value as JsonValue};

        Ok(match value {
            Value::Null => JsonValue::Null,

            // Numeric types that fit in JSON numbers
            Value::Int8(i) => JsonValue::Number(Number::from(i)),
            Value::Int16(i) => JsonValue::Number(Number::from(i)),
            Value::Int32(i) => JsonValue::Number(Number::from(i)),
            Value::Int64(i) => JsonValue::Number(Number::from(i)),
            Value::UInt8(i) => JsonValue::Number(Number::from(i)),
            Value::UInt16(i) => JsonValue::Number(Number::from(i)),
            Value::UInt32(i) => JsonValue::Number(Number::from(i)),
            Value::UInt64(i) => JsonValue::Number(Number::from(i)),

            // Large integers as strings
            Value::Int128(i) => JsonValue::String(i.to_string()),
            Value::Int256(i) => JsonValue::String(i.to_string()),
            Value::UInt128(i) => JsonValue::String(i.to_string()),
            Value::UInt256(i) => JsonValue::String(i.to_string()),

            // Floats
            Value::Float32(f) => {
                Number::from_f64(f64::from(f)).map_or(JsonValue::Null, JsonValue::Number)
            }
            Value::Float64(f) => Number::from_f64(f).map_or(JsonValue::Null, JsonValue::Number),

            // String
            Value::String(bytes) => JsonValue::String(
                String::from_utf8(bytes)
                    .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8: {e}")))?,
            ),

            // Everything else as debug string
            _ => JsonValue::String(format!("{value:?}")),
        })
    }

    /// Parse type entry from bytes
    fn parse_type_entry(type_name_bytes: Vec<u8>) -> Result<(String, Type)> {
        let type_name = String::from_utf8(type_name_bytes)
            .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}")))?;
        let typ = type_name
            .parse::<Type>()
            .map_err(|_| Error::DeserializeError(format!("Unknown type: {type_name}")))?;
        Ok((type_name, typ))
    }

    /// Build offsets and count rows per type
    fn build_offsets(
        discriminators: &[u64],
        total_types: u64,
    ) -> (Vec<usize>, HashMap<u64, usize>) {
        let mut row_count_by_type = HashMap::new();
        let mut offsets = vec![0; discriminators.len()];

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != total_types {
                let count = row_count_by_type.entry(disc).or_default();
                offsets[i] = *count;
                *count += 1;
            }
        }

        (offsets, row_count_by_type)
    }

    /// Reconstruct values from columns
    fn reconstruct_path_values(
        discriminators: &[u64],
        offsets: &[usize],
        columns: &HashMap<u64, Vec<Value>>,
        total_types: u64,
        rows: usize,
    ) -> Vec<Value> {
        let mut values = Vec::with_capacity(rows);

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == total_types {
                values.push(Value::Null);
            } else if let Some(column) = columns.get(&disc) {
                let offset = offsets[i];
                values.push(column.get(offset).cloned().unwrap_or(Value::Null));
            } else {
                values.push(Value::Null);
            }
        }

        values
    }

    /// Build JSON objects from path values
    fn build_json_objects(
        path_names: &[String],
        path_values: &HashMap<String, Vec<Value>>,
        rows: usize,
    ) -> Result<Vec<Value>> {
        let mut result = Vec::with_capacity(rows);

        for row_idx in 0..rows {
            let mut row_object = serde_json::Map::new();

            for path_name in path_names {
                if let Some(path_column) = path_values.get(path_name)
                    && let Some(value) = path_column.get(row_idx)
                    && !matches!(value, Value::Null)
                {
                    Self::set_nested_value(&mut row_object, path_name, value.clone())?;
                }
            }

            let json_string = serde_json::to_string(&serde_json::Value::Object(row_object))
                .map_err(|e| Error::DeserializeError(format!("Failed to serialize JSON: {e}")))?;
            result.push(Value::String(json_string.into_bytes()));
        }

        Ok(result)
    }
}

impl Deserializer for JsonDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.read_u64_le().await?;

        match version {
            JSON_STRING_VERSION => {
                // Simple string version - no paths or dynamic data
                state.type_specific = TypeSpecificState::Json(JsonStateData {
                    version: Some(version),
                    paths: Vec::new(),
                    path_columns: None,
                    rows: None,
                    dynamic_data: None,
                });
                Ok(())
            }
            JSON_OBJECT_VERSION_2 | JSON_OBJECT_VERSION_3 => {
                // Read total dynamic paths
                let total_paths = reader.read_var_uint().await?;

                // Read path names
                let mut path_names =
                    Vec::with_capacity(total_paths.try_into().unwrap_or(usize::MAX));
                for _ in 0..total_paths {
                    let path_bytes = reader.read_string().await?;
                    let path_name = String::from_utf8(path_bytes).map_err(|e| {
                        Error::DeserializeError(format!("Invalid UTF-8 in path: {e}"))
                    })?;
                    path_names.push(path_name);
                }

                // Read Dynamic headers for each path
                let mut dynamic_data = Vec::with_capacity(path_names.len());
                for path_name in &path_names {
                    // Read Dynamic version
                    let dyn_version = reader.read_u64_le().await?;
                    if dyn_version != 3 {
                        return Err(Error::DeserializeError(format!(
                            "Expected Dynamic v3 for path '{path_name}', got {dyn_version}"
                        )));
                    }

                    // Read types
                    let total_types = reader.read_var_uint().await?;
                    let mut types =
                        Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
                    for _ in 0..total_types {
                        types.push(Self::parse_type_entry(reader.read_string().await?)?);
                    }

                    // Read prefixes
                    for (_, typ) in &types {
                        typ.deserialize_prefix_async(reader, state).await?;
                    }

                    dynamic_data.push((total_types, types));
                }

                // Store metadata in state
                state.type_specific = TypeSpecificState::Json(JsonStateData {
                    version: Some(version),
                    paths: path_names,
                    path_columns: None,
                    rows: None,
                    dynamic_data: Some(dynamic_data),
                });
                Ok(())
            }
            _ => Err(Error::DeserializeError(format!(
                "Unsupported JSON version: {version}. Expected 1, 2, or 3."
            ))),
        }
    }

    async fn read<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, path_names, dynamic_data) = if let TypeSpecificState::Json(json_state) = &state.type_specific {
            let version = json_state.version.ok_or_else(|| {
                Error::DeserializeError("JSON version not set. read_prefix must be called first".to_string())
            })?;
            (version, json_state.paths.clone(), json_state.dynamic_data.clone())
        } else {
            return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
        };

        match version {
            JSON_STRING_VERSION => {
                // Simple string serialization
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    out.push(Value::String(reader.read_string().await?));
                }
                Ok(out)
            }
            JSON_OBJECT_VERSION_2 | JSON_OBJECT_VERSION_3 => {
                let dynamic_data = dynamic_data.ok_or_else(|| {
                    Error::DeserializeError("JSON object data not set".to_string())
                })?;

                // Read values for each path
                let mut path_values = HashMap::new();

                for (path_idx, path_name) in path_names.iter().enumerate() {
                    let (total_types, types) = &dynamic_data[path_idx];

                    // Read discriminators
                    let mut discriminators = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        discriminators.push(read_discriminator!(async reader, *total_types));
                    }

                    // Build offsets
                    let (offsets, row_count_by_type) =
                        Self::build_offsets(&discriminators, *total_types);

                    // Read column data
                    let mut columns = HashMap::new();
                    for (idx, (_, typ)) in types.iter().enumerate() {
                        let type_idx = idx as u64;
                        if let Some(&count) = row_count_by_type.get(&type_idx)
                            && count > 0
                        {
                            let values = typ.deserialize_column(reader, count, state).await?;
                            let old = columns.insert(type_idx, values);
                            debug_assert!(old.is_none());
                        }
                    }

                    // Reconstruct values
                    let values = Self::reconstruct_path_values(
                        &discriminators,
                        &offsets,
                        &columns,
                        *total_types,
                        rows,
                    );
                    let old = path_values.insert(path_name.clone(), values);
                    debug_assert!(old.is_none());
                }

                Self::build_json_objects(&path_names, &path_values, rows)
            }
            _ => Err(Error::DeserializeError(format!("Invalid JSON version: {version}"))),
        }
    }

    fn read_sync(
        _type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, path_names, dynamic_data) = if let TypeSpecificState::Json(json_state) = &state.type_specific {
            let version = json_state.version.ok_or_else(|| {
                Error::DeserializeError("JSON version not set. read_prefix must be called first".to_string())
            })?;
            (version, json_state.paths.clone(), json_state.dynamic_data.clone())
        } else {
            return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
        };

        match version {
            JSON_STRING_VERSION => {
                // Simple string serialization
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    out.push(Value::String(reader.try_get_string()?.to_vec()));
                }
                Ok(out)
            }
            JSON_OBJECT_VERSION_2 | JSON_OBJECT_VERSION_3 => {
                let dynamic_data = dynamic_data.ok_or_else(|| {
                    Error::DeserializeError("JSON object data not set".to_string())
                })?;

                // Read values for each path
                let mut path_values = HashMap::new();

                for (path_idx, path_name) in path_names.iter().enumerate() {
                    let (total_types, types) = &dynamic_data[path_idx];

                    // Read discriminators
                    let mut discriminators = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        discriminators.push(read_discriminator!(sync reader, *total_types));
                    }

                    // Build offsets
                    let (offsets, row_count_by_type) =
                        Self::build_offsets(&discriminators, *total_types);

                    // Read column data
                    let mut columns = HashMap::new();
                    for (idx, (_, typ)) in types.iter().enumerate() {
                        let type_idx = idx as u64;
                        if let Some(&count) = row_count_by_type.get(&type_idx)
                            && count > 0
                        {
                            let values = typ.deserialize_column_sync(reader, count, state)?;
                            let old = columns.insert(type_idx, values);
                            debug_assert!(old.is_none());
                        }
                    }

                    // Reconstruct values
                    let values = Self::reconstruct_path_values(
                        &discriminators,
                        &offsets,
                        &columns,
                        *total_types,
                        rows,
                    );
                    let old = path_values.insert(path_name.clone(), values);
                    debug_assert!(old.is_none());
                }

                Self::build_json_objects(&path_names, &path_values, rows)
            }
            _ => Err(Error::DeserializeError(format!("Invalid JSON version: {version}"))),
        }
    }
}
