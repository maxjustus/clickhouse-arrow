use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::{Error, Result};

pub(crate) struct JsonDeserializer;

impl JsonDeserializer {
    /// Set a value at a nested path in a JSON object map
    /// Path format: "user.name" -> creates nested structure {user: {name: value}}
    fn set_nested_value(
        object: &mut serde_json::Map<String, serde_json::Value>,
        path: &str,
        value: Value,
    ) -> Result<()> {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() {
            return Err(Error::DeserializeError("Empty path".to_string()));
        }

        let mut current = object;

        // Navigate to the nested location, creating objects as needed
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

        // Set the final value
        let final_key = parts[parts.len() - 1];
        let json_value = Self::value_to_json_value(value)?;
        drop(current.insert(final_key.to_string(), json_value));

        Ok(())
    }

    /// Convert a `ClickHouse` Value to a `serde_json::Value`
    fn value_to_json_value(value: Value) -> Result<serde_json::Value> {
        let json_value = match value {
            Value::Null => serde_json::Value::Null,
            Value::Int8(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::Int16(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::Int32(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::Int64(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::Int128(i) => serde_json::Value::String(i.to_string()), /* Too large for JSON */
            // number
            Value::Int256(i) => serde_json::Value::String(i.to_string()), /* Too large for JSON */
            // number
            Value::UInt8(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::UInt16(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::UInt32(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::UInt64(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            Value::UInt128(i) => serde_json::Value::String(i.to_string()), /* Too large for JSON */
            // number
            Value::UInt256(i) => serde_json::Value::String(i.to_string()), /* Too large for JSON */
            // number
            Value::Float32(f) => serde_json::Number::from_f64(f64::from(f))
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Value::Float64(f) => serde_json::Number::from_f64(f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Value::String(bytes) => {
                let s = String::from_utf8(bytes)
                    .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 string: {e}")))?;
                serde_json::Value::String(s)
            }
            Value::Decimal32(_, _)
            | Value::Decimal64(_, _)
            | Value::Decimal128(_, _)
            | Value::Decimal256(_, _) => {
                // Convert decimals to strings for now
                serde_json::Value::String(format!("{value:?}"))
            }
            // For complex types, convert to string representation for now
            _ => serde_json::Value::String(format!("{value:?}")),
        };
        Ok(json_value)
    }
}

// JSON serialization versions from ClickHouse
const JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION: u64 = 0;
const JSON_STRING_SERIALIZATION_VERSION: u64 = 1;
const JSON_OBJECT_SERIALIZATION_VERSION_2: u64 = 2; // Intermediate version, treat as object
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;

// Thread-local storage for JSON version
thread_local! {
    static JSON_VERSION: std::cell::RefCell<u64> = const { std::cell::RefCell::new(JSON_STRING_SERIALIZATION_VERSION) };
}

// Thread-local storage for JSON object serialization data
// Format: (total_dynamic_paths, path_names, dynamic_data)
// where dynamic_data is Vec<(total_types, types)> for each path
type JsonDynamicData = (u64, Vec<String>, Vec<(u64, Vec<(String, Type)>)>);
thread_local! {
    static JSON_DYNAMIC_DATA: std::cell::RefCell<Option<JsonDynamicData>> = const { std::cell::RefCell::new(None) };
}

impl Deserializer for JsonDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // Read the JSON serialization version as bytes for debugging
        let mut version_bytes = [0u8; 8];
        let _ = reader.read_exact(&mut version_bytes).await?;
        let version = u64::from_le_bytes(version_bytes);

        // Store the version for use in the read phase

        // Store the version in thread-local storage
        JSON_VERSION.with(|v| *v.borrow_mut() = version);

        match version {
            JSON_STRING_SERIALIZATION_VERSION => {
                // No additional prefix data for string serialization
                Ok(())
            }
            JSON_OBJECT_SERIALIZATION_VERSION_2 | JSON_OBJECT_SERIALIZATION_VERSION => {
                // JSON v3 object serialization format:
                // 1. Total dynamic paths (var_uint)
                // 2. For each path: path name (string)
                // 3. For each path: Dynamic column header (same as Dynamic v3)

                // Read total dynamic paths
                let total_dynamic_paths = reader.read_var_uint().await?;

                // Read path names
                let mut path_names = Vec::with_capacity(total_dynamic_paths as usize);
                for _ in 0..total_dynamic_paths {
                    let path_name_bytes = reader.read_string().await?;
                    let path_name = String::from_utf8(path_name_bytes).map_err(|e| {
                        Error::DeserializeError(format!("Invalid UTF-8 in path name: {e}"))
                    })?;
                    path_names.push(path_name);
                }

                // For each dynamic path, read the Dynamic column header
                let mut dynamic_data = Vec::with_capacity(total_dynamic_paths as usize);
                for path_name in &path_names {
                    // Each dynamic path has its own Dynamic column with header
                    // Read Dynamic version (should be 3)
                    let dynamic_version = reader.read_u64_le().await?;
                    if dynamic_version != 3 {
                        return Err(Error::DeserializeError(format!(
                            "Expected Dynamic version 3 for JSON path '{path_name}', got \
                             {dynamic_version}"
                        )));
                    }

                    // Read Dynamic header (same as Dynamic v3 format)
                    let total_types = reader.read_var_uint().await?;
                    let mut types = Vec::with_capacity(total_types as usize);
                    for _ in 0..total_types {
                        let type_name_bytes = reader.read_string().await?;
                        let type_name = String::from_utf8(type_name_bytes).map_err(|e| {
                            Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}"))
                        })?;
                        let typ = type_name.parse::<Type>().map_err(|_| {
                            Error::DeserializeError(format!("Unknown type: {type_name}"))
                        })?;
                        types.push((type_name, typ));
                    }

                    // Read prefixes for nested types in this Dynamic column
                    for (_, typ) in &types {
                        typ.deserialize_prefix_async(reader, state).await?;
                    }

                    dynamic_data.push((total_types, types));
                }

                // Store all the data for the read phase
                JSON_VERSION.with(|v| *v.borrow_mut() = version);
                JSON_DYNAMIC_DATA.with(|data| {
                    *data.borrow_mut() = Some((total_dynamic_paths, path_names, dynamic_data));
                });

                Ok(())
            }
            JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION => {
                // TODO: Handle deprecated object serialization
                Err(Error::DeserializeError(
                    "Deprecated JSON object serialization not yet supported".to_string(),
                ))
            }
            _ => Err(Error::DeserializeError(format!(
                "Unsupported JSON serialization version: {version}. Expected 1 (string), 2 \
                 (object v2), or 3 (object v3)."
            ))),
        }
    }

    async fn read<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Get the version from thread-local storage
        let version = JSON_VERSION.with(|v| *v.borrow());

        match version {
            JSON_STRING_SERIALIZATION_VERSION => {
                // Read JSON values as strings
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    let json_bytes = reader.read_string().await?;
                    // For JSON string serialization, we store the raw JSON as string bytes
                    out.push(Value::String(json_bytes));
                }
                Ok(out)
            }
            JSON_OBJECT_SERIALIZATION_VERSION_2 | JSON_OBJECT_SERIALIZATION_VERSION => {
                // Get the stored JSON format data from prefix phase
                let (_total_dynamic_paths, path_names, dynamic_data) =
                    JSON_DYNAMIC_DATA.with(|data| {
                        data.borrow().clone().ok_or_else(|| {
                            Error::DeserializeError("JSON object data not set in state".to_string())
                        })
                    })?;

                use std::collections::HashMap;

                // For each dynamic path, read its data using Dynamic format
                let mut path_values: HashMap<String, Vec<Value>> = HashMap::new();

                for (path_idx, path_name) in path_names.iter().enumerate() {
                    let (total_types, types) = &dynamic_data[path_idx];

                    // Read discriminators for this path's Dynamic column
                    let mut discriminators = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        let disc = match total_types {
                            0..=255 => u64::from(reader.read_u8().await?),
                            256..=65535 => u64::from(reader.read_u16_le().await?),
                            65536..=4_294_967_295 => u64::from(reader.read_u32_le().await?),
                            _ => reader.read_u64_le().await?,
                        };
                        discriminators.push(disc);
                    }

                    // Count rows per type for this path
                    let mut row_count_by_type: HashMap<u64, usize> = HashMap::new();
                    let mut offsets = vec![0; rows];

                    for (i, &disc) in discriminators.iter().enumerate() {
                        if disc != *total_types {
                            let count = row_count_by_type.entry(disc).or_insert(0);
                            offsets[i] = *count;
                            *count += 1;
                        }
                    }

                    // Read column data for each type in this path
                    let mut columns: HashMap<u64, Vec<Value>> = HashMap::new();

                    for (idx, (_, typ)) in types.iter().enumerate() {
                        let type_idx = idx as u64;
                        if let Some(&count) = row_count_by_type.get(&type_idx) {
                            if count > 0 {
                                let column_values =
                                    typ.deserialize_column(reader, count, state).await?;
                                drop(columns.insert(type_idx, column_values));
                            }
                        }
                    }

                    // Reconstruct values for this path
                    let mut path_column_values = Vec::with_capacity(rows);
                    for (i, &disc) in discriminators.iter().enumerate() {
                        if disc == *total_types {
                            path_column_values.push(Value::Null);
                        } else if let Some(column) = columns.get(&disc) {
                            let offset = offsets[i];
                            if offset < column.len() {
                                path_column_values.push(column[offset].clone());
                            } else {
                                // This case can be hit with sparse data, where a discriminator
                                // exists but there are no corresponding values. It should be null.
                                path_column_values.push(Value::Null);
                            }
                        } else {
                            // This case can also be hit with sparse data.
                            path_column_values.push(Value::Null);
                        }
                    }

                    drop(path_values.insert(path_name.clone(), path_column_values));
                }

                // Build JSON objects for each row by combining all paths
                let mut result_values = Vec::with_capacity(rows);

                for row_idx in 0..rows {
                    // Create a nested map for this row using all paths
                    let mut row_object = serde_json::Map::new();

                    for path_name in &path_names {
                        if let Some(path_column) = path_values.get(path_name) {
                            if row_idx < path_column.len() {
                                let value = &path_column[row_idx];
                                if !matches!(value, Value::Null) {
                                    // Split path by '.' and build nested structure
                                    Self::set_nested_value(
                                        &mut row_object,
                                        path_name,
                                        value.clone(),
                                    )?;
                                }
                            }
                        }
                    }

                    // Convert the nested map to a JSON Value, then to our Value type
                    let json_value = serde_json::Value::Object(row_object);
                    let json_string = serde_json::to_string(&json_value).map_err(|e| {
                        Error::DeserializeError(format!("Failed to serialize JSON object: {e}"))
                    })?;

                    result_values.push(Value::String(json_string.into_bytes()));
                }

                Ok(result_values)
            }
            _ => Err(Error::DeserializeError(format!(
                "Unsupported JSON serialization version during read: {version}"
            ))),
        }
    }

    fn read_sync(
        _type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Get the version from thread-local storage
        let version = JSON_VERSION.with(|v| *v.borrow());

        match version {
            JSON_STRING_SERIALIZATION_VERSION => {
                // Read JSON values as strings
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    let json_bytes = reader.try_get_string()?;
                    out.push(Value::String(json_bytes.to_vec()));
                }
                Ok(out)
            }
            JSON_OBJECT_SERIALIZATION_VERSION_2 | JSON_OBJECT_SERIALIZATION_VERSION => {
                // Get the stored JSON format data from prefix phase
                let (_total_dynamic_paths, path_names, dynamic_data) =
                    JSON_DYNAMIC_DATA.with(|data| {
                        data.borrow().clone().ok_or_else(|| {
                            Error::DeserializeError("JSON object data not set in state".to_string())
                        })
                    })?;

                use std::collections::HashMap;

                // For each dynamic path, read its data using Dynamic format
                let mut path_values: HashMap<String, Vec<Value>> = HashMap::new();

                for (path_idx, path_name) in path_names.iter().enumerate() {
                    let (total_types, types) = &dynamic_data[path_idx];

                    // Read discriminators for this path's Dynamic column
                    let mut discriminators = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        let disc = match total_types {
                            0..=255 => u64::from(reader.get_u8()),
                            256..=65535 => u64::from(reader.get_u16_le()),
                            65536..=4_294_967_295 => u64::from(reader.get_u32_le()),
                            _ => reader.get_u64_le(),
                        };
                        discriminators.push(disc);
                    }

                    // Count rows per type for this path
                    let mut row_count_by_type: HashMap<u64, usize> = HashMap::new();
                    let mut offsets = vec![0; rows];

                    for (i, &disc) in discriminators.iter().enumerate() {
                        if disc != *total_types {
                            let count = row_count_by_type.entry(disc).or_insert(0);
                            offsets[i] = *count;
                            *count += 1;
                        }
                    }

                    // Read column data for each type in this path
                    let mut columns: HashMap<u64, Vec<Value>> = HashMap::new();

                    for (idx, (_, typ)) in types.iter().enumerate() {
                        let type_idx = idx as u64;
                        if let Some(&count) = row_count_by_type.get(&type_idx) {
                            if count > 0 {
                                let column_values =
                                    typ.deserialize_column_sync(reader, count, state)?;
                                drop(columns.insert(type_idx, column_values));
                            }
                        }
                    }

                    // Reconstruct values for this path
                    let mut path_column_values = Vec::with_capacity(rows);
                    for (i, &disc) in discriminators.iter().enumerate() {
                        if disc == *total_types {
                            path_column_values.push(Value::Null);
                        } else if let Some(column) = columns.get(&disc) {
                            let offset = offsets[i];
                            if offset < column.len() {
                                path_column_values.push(column[offset].clone());
                            } else {
                                // This case can be hit with sparse data, where a discriminator
                                // exists but there are no corresponding values. It should be null.
                                path_column_values.push(Value::Null);
                            }
                        } else {
                            // This case can also be hit with sparse data.
                            path_column_values.push(Value::Null);
                        }
                    }

                    drop(path_values.insert(path_name.clone(), path_column_values));
                }

                // Build JSON objects for each row by combining all paths
                let mut result_values = Vec::with_capacity(rows);

                for row_idx in 0..rows {
                    // Create a nested map for this row using all paths
                    let mut row_object = serde_json::Map::new();

                    for path_name in &path_names {
                        if let Some(path_column) = path_values.get(path_name) {
                            if row_idx < path_column.len() {
                                let value = &path_column[row_idx];
                                if !matches!(value, Value::Null) {
                                    // Split path by '.' and build nested structure
                                    Self::set_nested_value(
                                        &mut row_object,
                                        path_name,
                                        value.clone(),
                                    )?;
                                }
                            }
                        }
                    }

                    // Convert the nested map to a JSON Value, then to our Value type
                    let json_value = serde_json::Value::Object(row_object);
                    let json_string = serde_json::to_string(&json_value).map_err(|e| {
                        Error::DeserializeError(format!("Failed to serialize JSON object: {e}"))
                    })?;

                    result_values.push(Value::String(json_string.into_bytes()));
                }

                Ok(result_values)
            }
            _ => Err(Error::DeserializeError(format!(
                "Unsupported JSON serialization version during sync read: {version}"
            ))),
        }
    }
}
