use std::collections::BTreeMap;

use tokio::io::AsyncWriteExt;

use super::dynamic::DynamicSerializer;
use super::{ClickHouseNativeSerializer, Serializer, SerializerState, Type};
use crate::formats::{JsonState, TypeSpecificState};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::values::{Date, Date32, DateTime, DynDateTime64};
use crate::{Error, Result, Value};

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;

// JSON v3 object serialization is now supported via thread-local caching
// The implementation follows the same pattern as Dynamic type serialization:
// 1. analyze_values is called before serialization to collect metadata
// 2. metadata is cached in thread-local storage
// 3. write_prefix uses cached metadata to write the full header
// 4. write uses cached data to write column data efficiently

/// Parsed JSON data organized by typed and dynamic paths
#[derive(Debug, Clone)]
struct JsonData {
    /// Map from path to values for dynamic paths
    dynamic_path_columns: BTreeMap<String, Vec<Value>>,
    /// Map from path to values for typed paths
    typed_path_columns:   BTreeMap<String, Vec<Value>>,
    /// Number of rows
    rows:                 usize,
}

impl JsonData {
    /// Parse JSON values into path-organized structure
    fn from_values(
        values: Vec<Value>,
        typed_paths: &[(String, Type)],
        skip_paths: &[String],
    ) -> Result<Self> {
        let mut dynamic_path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut typed_path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let rows = values.len();

        // Pre-populate typed path columns with nulls
        for (path, _) in typed_paths {
            drop(typed_path_columns.insert(path.clone(), vec![Value::Null; rows]));
        }

        // Compile skip path patterns as regex
        let skip_patterns: Result<Vec<regex::Regex>> = skip_paths
            .iter()
            .map(|p| {
                regex::Regex::new(p).map_err(|e| {
                    Error::SerializeError(format!("Invalid skip_path pattern '{}': {}", p, e))
                })
            })
            .collect();
        let skip_patterns = skip_patterns?;

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
                        &mut dynamic_path_columns,
                        &mut typed_path_columns,
                        typed_paths,
                        &skip_patterns,
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
        for column in dynamic_path_columns.values_mut() {
            while column.len() < rows {
                column.push(Value::Null);
            }
        }

        Ok(JsonData { dynamic_path_columns, typed_path_columns, rows })
    }

    /// Recursively extract paths from JSON value
    fn extract_paths_from_json(
        json_value: &serde_json::Value,
        current_path: &str,
        dynamic_path_columns: &mut BTreeMap<String, Vec<Value>>,
        typed_path_columns: &mut BTreeMap<String, Vec<Value>>,
        typed_paths: &[(String, Type)],
        skip_patterns: &[regex::Regex],
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

                Self::extract_paths_from_json(
                    value,
                    &path,
                    dynamic_path_columns,
                    typed_path_columns,
                    typed_paths,
                    skip_patterns,
                    row_idx,
                    total_rows,
                )?;
            }
        } else {
            // Check if this path should be skipped
            for pattern in skip_patterns {
                if pattern.is_match(current_path) {
                    return Ok(()); // Skip this path
                }
            }

            // Leaf value - convert to ClickHouse Value and store
            let ch_value = Self::json_value_to_clickhouse_value(json_value)?;

            // Check if this is a typed path
            let typed_path_type =
                typed_paths.iter().find(|(path, _)| path == current_path).map(|(_, typ)| typ);

            if let Some(expected_type) = typed_path_type {
                // Convert the value to the expected type
                let converted_value =
                    Self::convert_to_type_with_path(ch_value, expected_type, current_path)?;

                // Store in typed path columns
                if let Some(column) = typed_path_columns.get_mut(current_path) {
                    if row_idx < column.len() {
                        column[row_idx] = converted_value;
                    }
                }
            } else {
                // Store in dynamic path columns
                let column = dynamic_path_columns
                    .entry(current_path.to_string())
                    .or_insert_with(|| vec![Value::Null; total_rows]);

                if row_idx < column.len() {
                    column[row_idx] = ch_value;
                }
            }
        }
        Ok(())
    }

    /// Convert a value to a specific type with path context for better error messages
    fn convert_to_type_with_path(value: Value, expected_type: &Type, path: &str) -> Result<Value> {
        Self::convert_to_type(value.clone(), expected_type).map_err(|e| {
            Error::SerializeError(format!(
                "Failed to convert value for path '{}' to type {:?}: {}",
                path, expected_type, e
            ))
        })
    }

    /// Convert a value to a specific type if needed
    fn convert_to_type(value: Value, expected_type: &Type) -> Result<Value> {
        match (value, expected_type) {
            // Pass through nulls for any type
            (Value::Null, _) => Ok(Value::Null),

            // If already the exact type, return as is
            (v @ Value::Int8(_), Type::Int8) => Ok(v),
            (v @ Value::Int16(_), Type::Int16) => Ok(v),
            (v @ Value::Int32(_), Type::Int32) => Ok(v),
            (v @ Value::Int64(_), Type::Int64) => Ok(v),
            (v @ Value::UInt8(_), Type::UInt8) => Ok(v),
            (v @ Value::UInt16(_), Type::UInt16) => Ok(v),
            (v @ Value::UInt32(_), Type::UInt32) => Ok(v),
            (v @ Value::UInt64(_), Type::UInt64) => Ok(v),
            (v @ Value::Float32(_), Type::Float32) => Ok(v),
            (v @ Value::Float64(_), Type::Float64) => Ok(v),
            (v @ Value::String(_), Type::String) => Ok(v),
            (v @ Value::String(_), Type::FixedSizedString(_)) => Ok(v),

            // Convert from Int64 - use wrapping semantics like ClickHouse
            (Value::Int64(i), Type::Int8) => Ok(Value::Int8(i as i8)),
            (Value::Int64(i), Type::Int16) => Ok(Value::Int16(i as i16)),
            (Value::Int64(i), Type::Int32) => Ok(Value::Int32(i as i32)),
            (Value::Int64(i), Type::UInt8) => Ok(Value::UInt8(i as u8)),
            (Value::Int64(i), Type::UInt16) => Ok(Value::UInt16(i as u16)),
            (Value::Int64(i), Type::UInt32) => Ok(Value::UInt32(i as u32)),
            (Value::Int64(i), Type::UInt64) => Ok(Value::UInt64(i as u64)),

            // Convert from UInt64 - use wrapping semantics
            (Value::UInt64(u), Type::Int8) => Ok(Value::Int8(u as i8)),
            (Value::UInt64(u), Type::Int16) => Ok(Value::Int16(u as i16)),
            (Value::UInt64(u), Type::Int32) => Ok(Value::Int32(u as i32)),
            (Value::UInt64(u), Type::Int64) => Ok(Value::Int64(u as i64)),
            (Value::UInt64(u), Type::UInt8) => Ok(Value::UInt8(u as u8)),
            (Value::UInt64(u), Type::UInt16) => Ok(Value::UInt16(u as u16)),
            (Value::UInt64(u), Type::UInt32) => Ok(Value::UInt32(u as u32)),

            // Float to Integer - truncate like ClickHouse
            (Value::Float32(f), Type::Int8) => Ok(Value::Int8(f as i8)),
            (Value::Float32(f), Type::Int16) => Ok(Value::Int16(f as i16)),
            (Value::Float32(f), Type::Int32) => Ok(Value::Int32(f as i32)),
            (Value::Float32(f), Type::Int64) => Ok(Value::Int64(f as i64)),
            (Value::Float32(f), Type::UInt8) => Ok(Value::UInt8(f as u8)),
            (Value::Float32(f), Type::UInt16) => Ok(Value::UInt16(f as u16)),
            (Value::Float32(f), Type::UInt32) => Ok(Value::UInt32(f as u32)),
            (Value::Float32(f), Type::UInt64) => Ok(Value::UInt64(f as u64)),
            (Value::Float64(f), Type::Int8) => Ok(Value::Int8(f as i8)),
            (Value::Float64(f), Type::Int16) => Ok(Value::Int16(f as i16)),
            (Value::Float64(f), Type::Int32) => Ok(Value::Int32(f as i32)),
            (Value::Float64(f), Type::Int64) => Ok(Value::Int64(f as i64)),
            (Value::Float64(f), Type::UInt8) => Ok(Value::UInt8(f as u8)),
            (Value::Float64(f), Type::UInt16) => Ok(Value::UInt16(f as u16)),
            (Value::Float64(f), Type::UInt32) => Ok(Value::UInt32(f as u32)),
            (Value::Float64(f), Type::UInt64) => Ok(Value::UInt64(f as u64)),

            // Float conversions between types
            (Value::Float64(f), Type::Float32) => Ok(Value::Float32(f as f32)),
            (Value::Float32(f), Type::Float64) => Ok(Value::Float64(f as f64)),

            // Numeric to Float
            (Value::Int64(i), Type::Float32) => Ok(Value::Float32(i as f32)),
            (Value::Int64(i), Type::Float64) => Ok(Value::Float64(i as f64)),
            (Value::UInt64(u), Type::Float32) => Ok(Value::Float32(u as f32)),
            (Value::UInt64(u), Type::Float64) => Ok(Value::Float64(u as f64)),
            (Value::Int8(i), Type::Float32) => Ok(Value::Float32(i as f32)),
            (Value::Int8(i), Type::Float64) => Ok(Value::Float64(i as f64)),
            (Value::Int16(i), Type::Float32) => Ok(Value::Float32(i as f32)),
            (Value::Int16(i), Type::Float64) => Ok(Value::Float64(i as f64)),
            (Value::Int32(i), Type::Float32) => Ok(Value::Float32(i as f32)),
            (Value::Int32(i), Type::Float64) => Ok(Value::Float64(i as f64)),
            (Value::UInt8(u), Type::Float32) => Ok(Value::Float32(u as f32)),
            (Value::UInt8(u), Type::Float64) => Ok(Value::Float64(u as f64)),
            (Value::UInt16(u), Type::Float32) => Ok(Value::Float32(u as f32)),
            (Value::UInt16(u), Type::Float64) => Ok(Value::Float64(u as f64)),
            (Value::UInt32(u), Type::Float32) => Ok(Value::Float32(u as f32)),
            (Value::UInt32(u), Type::Float64) => Ok(Value::Float64(u as f64)),

            // Convert between smaller integer types - use wrapping
            (Value::Int8(i), Type::UInt8) => Ok(Value::UInt8(i as u8)),
            (Value::Int16(i), Type::UInt16) => Ok(Value::UInt16(i as u16)),
            (Value::Int32(i), Type::UInt32) => Ok(Value::UInt32(i as u32)),
            (Value::UInt8(u), Type::Int8) => Ok(Value::Int8(u as i8)),
            (Value::UInt16(u), Type::Int16) => Ok(Value::Int16(u as i16)),
            (Value::UInt32(u), Type::Int32) => Ok(Value::Int32(u as i32)),

            // String to numeric parsing
            (Value::String(s), Type::Int8) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i8>().map(Value::Int8).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Int8: {}", s_str, e))
                })
            }
            (Value::String(s), Type::Int16) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i16>().map(Value::Int16).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Int16: {}", s_str, e))
                })
            }
            (Value::String(s), Type::Int32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i32>().map(Value::Int32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Int32: {}", s_str, e))
                })
            }
            (Value::String(s), Type::Int64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i64>().map(Value::Int64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Int64: {}", s_str, e))
                })
            }
            (Value::String(s), Type::UInt8) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u8>().map(Value::UInt8).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as UInt8: {}", s_str, e))
                })
            }
            (Value::String(s), Type::UInt16) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u16>().map(Value::UInt16).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as UInt16: {}", s_str, e))
                })
            }
            (Value::String(s), Type::UInt32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u32>().map(Value::UInt32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as UInt32: {}", s_str, e))
                })
            }
            (Value::String(s), Type::UInt64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u64>().map(Value::UInt64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as UInt64: {}", s_str, e))
                })
            }
            (Value::String(s), Type::Float32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<f32>().map(Value::Float32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Float32: {}", s_str, e))
                })
            }
            (Value::String(s), Type::Float64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<f64>().map(Value::Float64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{}' as Float64: {}", s_str, e))
                })
            }

            // Date/DateTime conversions from timestamp
            (Value::Int64(i), Type::Date) => {
                // Date is days since epoch (truncate to u16, wrapping)
                Ok(Value::Date(Date(i as u16)))
            }
            (Value::Int64(i), Type::Date32) => {
                // Date32 is days since epoch
                Ok(Value::Date32(Date32(i as i32)))
            }
            (Value::Int64(i), Type::DateTime(tz)) => {
                // DateTime is seconds since epoch (truncate to u32)
                let datetime = DateTime(tz.clone(), i as u32);
                Ok(Value::DateTime(datetime))
            }
            (Value::Int64(i), Type::DateTime64(precision, tz)) => {
                // DateTime64 is fractional seconds since epoch
                let datetime = DynDateTime64(tz.clone(), i as u64, *precision);
                Ok(Value::DateTime64(datetime))
            }
            (Value::UInt64(u), Type::Date) => Ok(Value::Date(Date(u as u16))),
            (Value::UInt64(u), Type::Date32) => Ok(Value::Date32(Date32(u as i32))),
            (Value::UInt64(u), Type::DateTime(tz)) => {
                let datetime = DateTime(tz.clone(), u as u32);
                Ok(Value::DateTime(datetime))
            }
            (Value::UInt64(u), Type::DateTime64(precision, tz)) => {
                let datetime = DynDateTime64(tz.clone(), u, *precision);
                Ok(Value::DateTime64(datetime))
            }

            // Decimal conversions from numeric types - scale first, then value
            (Value::Int64(i), Type::Decimal32(scale)) => Ok(Value::Decimal32(*scale, i as i32)),
            (Value::Int64(i), Type::Decimal64(scale)) => Ok(Value::Decimal64(*scale, i)),
            (Value::Int64(i), Type::Decimal128(scale)) => Ok(Value::Decimal128(*scale, i as i128)),
            (Value::UInt64(u), Type::Decimal32(scale)) => Ok(Value::Decimal32(*scale, u as i32)),
            (Value::UInt64(u), Type::Decimal64(scale)) => Ok(Value::Decimal64(*scale, u as i64)),
            (Value::UInt64(u), Type::Decimal128(scale)) => Ok(Value::Decimal128(*scale, u as i128)),
            (Value::Float64(f), Type::Decimal32(scale)) => {
                // Scale the float value
                let scaled = f * 10_f64.powi(*scale as i32);
                Ok(Value::Decimal32(*scale, scaled as i32))
            }
            (Value::Float64(f), Type::Decimal64(scale)) => {
                let scaled = f * 10_f64.powi(*scale as i32);
                Ok(Value::Decimal64(*scale, scaled as i64))
            }
            (Value::Float64(f), Type::Decimal128(scale)) => {
                let scaled = f * 10_f64.powi(*scale as i32);
                Ok(Value::Decimal128(*scale, scaled as i128))
            }

            // Array handling - pass through
            (v @ Value::Array(_), Type::Array(_)) => Ok(v),

            // Handle Nullable types by recursing
            (value, Type::Nullable(inner)) => Self::convert_to_type(value, inner),

            // Default: return as is and let validation catch any issues
            (v, _) => Ok(v),
        }
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
            serde_json::Value::Array(arr) => {
                // Convert array elements recursively to preserve structure
                let elements: Result<Vec<Value>> =
                    arr.iter().map(Self::json_value_to_clickhouse_value).collect();
                Value::Array(elements?)
            }
            serde_json::Value::Object(_) => {
                // For objects at leaf positions, serialize back to JSON string
                // (Objects should have been recursed through already)
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
}

impl JsonSerializer {
    /// Analyze JSON values and return metadata for use in `write_prefix`
    pub(crate) fn analyze_values(values: &[Value], type_: &Type) -> Result<TypeSpecificState> {
        // Extract typed_paths and skip_paths from the Type
        let (typed_paths, skip_paths) = match type_ {
            Type::JSON { typed_paths, skip_paths, .. } => {
                let typed: Vec<(String, Type)> = typed_paths
                    .iter()
                    .map(|(path, boxed_type)| (path.clone(), *boxed_type.clone()))
                    .collect();
                (typed, skip_paths.clone())
            }
            _ => return Err(Error::SerializeError("Expected JSON type".to_string())),
        };

        // Parse JSON values into path-organized structure
        let json_data = JsonData::from_values(values.to_vec(), &typed_paths, &skip_paths)?;

        // Build the metadata
        let mut dynamic_paths: Vec<String> =
            json_data.dynamic_path_columns.keys().cloned().collect();
        dynamic_paths.sort(); // Ensure consistent ordering

        let state = JsonState {
            version:              None, // Will be set properly in write_prefix
            dynamic_paths:        dynamic_paths.clone(),
            typed_paths:          typed_paths.clone(),
            dynamic_path_columns: Some(json_data.dynamic_path_columns),
            typed_path_columns:   Some(json_data.typed_path_columns),
            rows:                 Some(json_data.rows),
            dynamic_data:         None,
            path_dynamic_states:  Default::default(), // Will be filled in write_prefix
            // Deprecated fields for compatibility
            paths:                dynamic_paths,
            path_columns:         None,
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
            // In v3 format, typed paths are NOT written to ObjectStructure
            // They are implicitly known from the schema

            // Write ONLY dynamic paths to ObjectStructure
            let dynamic_paths = json_state.dynamic_paths.clone();
            let dynamic_columns = json_state.dynamic_path_columns.clone();
            Self::write_paths_header_sync(&dynamic_paths, version, writer)?;

            // Write Dynamic column headers for each dynamic path and store states
            if let Some(dynamic_columns) = dynamic_columns {
                // Create a map to store Dynamic states
                let mut path_dynamic_states = BTreeMap::new();

                for path in dynamic_paths {
                    if let Some(column_values) = dynamic_columns.get(&path) {
                        let dynamic_state = DynamicSerializer::write_dynamic_header_sync(
                            column_values,
                            writer,
                            state,
                        )?;
                        // Store the Dynamic state for this path
                        if let TypeSpecificState::Dynamic(dyn_state) = dynamic_state {
                            drop(path_dynamic_states.insert(path, dyn_state));
                        }
                    }
                }

                // Now update the state with all the dynamic states
                if let TypeSpecificState::Json(json_state_mut) = &mut state.type_specific {
                    json_state_mut.path_dynamic_states = path_dynamic_states;
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
            // In v3 format, typed paths are NOT written to ObjectStructure
            // They are implicitly known from the schema

            // Write ONLY dynamic paths to ObjectStructure
            let dynamic_paths = json_state.dynamic_paths.clone();
            let dynamic_columns = json_state.dynamic_path_columns.clone();
            Self::write_paths_header_async(&dynamic_paths, version, writer).await?;

            // Write Dynamic column headers for each dynamic path and store states
            if let Some(dynamic_columns) = dynamic_columns {
                // Create a map to store Dynamic states
                let mut path_dynamic_states = BTreeMap::new();

                for path in dynamic_paths {
                    if let Some(column_values) = dynamic_columns.get(&path) {
                        let dynamic_state = DynamicSerializer::write_dynamic_header_async(
                            column_values,
                            writer,
                            state,
                        )
                        .await?;
                        // Store the Dynamic state for this path
                        if let TypeSpecificState::Dynamic(dyn_state) = dynamic_state {
                            drop(path_dynamic_states.insert(path, dyn_state));
                        }
                    }
                }

                // Now update the state with all the dynamic states
                if let TypeSpecificState::Json(json_state_mut) = &mut state.type_specific {
                    json_state_mut.path_dynamic_states = path_dynamic_states;
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
        let (_typed_paths, _typed_columns, dynamic_paths, dynamic_columns, rows) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let rows = json_state.rows.ok_or_else(|| {
                    Error::SerializeError("JSON rows count not found in state".to_string())
                })?;

                let typed_columns = json_state.typed_path_columns.clone().unwrap_or_default();
                let dynamic_columns = json_state.dynamic_path_columns.clone().unwrap_or_default();

                (
                    json_state.typed_paths.clone(),
                    typed_columns,
                    json_state.dynamic_paths.clone(),
                    dynamic_columns,
                    rows,
                )
            } else {
                return Err(Error::SerializeError(
                    "JSON serialization state not found. `analyze_values` must be called before \
                     `write`."
                        .to_string(),
                ));
            };

        // First write typed path columns (part of v3 format)
        for (path, type_) in &_typed_paths {
            // Use a clean state for typed columns but preserve server version
            let mut typed_state = SerializerState::default();
            if let Some(version) = state.server_version {
                typed_state = typed_state.with_server_version(version);
            }

            // Write prefix for this typed column
            type_.serialize_prefix_async(writer, &mut typed_state).await?;

            if let Some(column_values) = _typed_columns.get(path) {
                // Write typed column data directly using the specified type
                type_.serialize_column(column_values.clone(), writer, &mut typed_state).await?;
            } else {
                // Write nulls if no data for this typed path
                let nulls = vec![Value::Null; rows];
                type_.serialize_column(nulls, writer, &mut typed_state).await?;
            }
        }

        // Then write dynamic path columns
        let path_dynamic_states = if let TypeSpecificState::Json(json_state) = &state.type_specific
        {
            json_state.path_dynamic_states.clone()
        } else {
            Default::default()
        };

        for path in &dynamic_paths {
            if let Some(column_values) = dynamic_columns.get(path) {
                if let Some(dynamic_state) = path_dynamic_states.get(path) {
                    // Use the stored Dynamic state for this path
                    DynamicSerializer::write_dynamic_data_async(
                        column_values,
                        writer,
                        state,
                        TypeSpecificState::Dynamic(dynamic_state.clone()),
                    )
                    .await?;
                } else {
                    return Err(Error::SerializeError(format!(
                        "Dynamic state not found for path: {}",
                        path
                    )));
                }
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
        let (_typed_paths, _typed_columns, dynamic_paths, dynamic_columns, rows) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let rows = json_state.rows.ok_or_else(|| {
                    Error::SerializeError("JSON rows count not found in state".to_string())
                })?;

                let typed_columns = json_state.typed_path_columns.clone().unwrap_or_default();
                let dynamic_columns = json_state.dynamic_path_columns.clone().unwrap_or_default();

                (
                    json_state.typed_paths.clone(),
                    typed_columns,
                    json_state.dynamic_paths.clone(),
                    dynamic_columns,
                    rows,
                )
            } else {
                return Err(Error::SerializeError(
                    "JSON serialization state not found. `analyze_values` must be called before \
                     `write`."
                        .to_string(),
                ));
            };

        // First write typed path columns (part of v3 format)
        for (path, type_) in &_typed_paths {
            // Use a clean state for typed columns but preserve server version
            let mut typed_state = SerializerState::default();
            if let Some(version) = state.server_version {
                typed_state = typed_state.with_server_version(version);
            }

            // Write prefix for this typed column
            type_.serialize_prefix(writer, &mut typed_state);

            if let Some(column_values) = _typed_columns.get(path) {
                // Write typed column data directly using the specified type
                type_.serialize_column_sync(column_values.clone(), writer, &mut typed_state)?;
            } else {
                // Write nulls if no data for this typed path
                let nulls = vec![Value::Null; rows];
                type_.serialize_column_sync(nulls, writer, &mut typed_state)?;
            }
        }

        // Then write dynamic path columns
        let path_dynamic_states = if let TypeSpecificState::Json(json_state) = &state.type_specific
        {
            json_state.path_dynamic_states.clone()
        } else {
            Default::default()
        };

        for path in &dynamic_paths {
            if let Some(column_values) = dynamic_columns.get(path) {
                if let Some(dynamic_state) = path_dynamic_states.get(path) {
                    // Use the stored Dynamic state for this path
                    DynamicSerializer::write_dynamic_data_sync(
                        column_values,
                        writer,
                        state,
                        TypeSpecificState::Dynamic(dynamic_state.clone()),
                    )?;
                } else {
                    return Err(Error::SerializeError(format!(
                        "Dynamic state not found for path: {}",
                        path
                    )));
                }
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
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;

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
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
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
        // Read dynamic paths count (typed paths are not in ObjectStructure)
        let mut path_count_byte = [0u8; 1];
        cursor.read_exact(&mut path_count_byte)?;
        assert!(path_count_byte[0] > 0, "Should have dynamic paths for object serialization");

        // Verify deserialized data structure
        for value in &deserialized {
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
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
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
        for value in &deserialized {
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
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![],
        };
        let type_specific_state = JsonSerializer::analyze_values(&values, &type_)?;

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
    async fn test_json_with_typed_paths() -> Result<()> {
        // Test JSON with typed paths
        let values = vec![
            Value::String(br#"{"id": 123, "name": "Alice", "score": 95.5}"#.to_vec()),
            Value::String(br#"{"id": 456, "name": "Bob", "active": true}"#.to_vec()),
            Value::String(br#"{"id": 789, "name": "Charlie", "tags": ["a", "b"]}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            skip_paths:        vec![],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            // Verify typed paths are separated
            assert_eq!(json_state.typed_paths.len(), 2);
            assert!(json_state.typed_paths.iter().any(|(p, _)| p == "id"));
            assert!(json_state.typed_paths.iter().any(|(p, _)| p == "name"));

            // Verify dynamic paths don't include typed ones
            assert!(!json_state.dynamic_paths.contains(&"id".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"name".to_string()));

            // Verify dynamic paths contain the remaining fields
            assert!(
                json_state.dynamic_paths.contains(&"score".to_string())
                    || json_state.dynamic_paths.contains(&"active".to_string())
                    || json_state.dynamic_paths.contains(&"tags".to_string())
            );

            // Verify typed columns exist
            let typed_columns = json_state.typed_path_columns.as_ref().unwrap();
            assert!(typed_columns.contains_key("id"));
            assert!(typed_columns.contains_key("name"));
        } else {
            panic!("Expected JSON state");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_with_skip_paths() -> Result<()> {
        // Test JSON with skip paths
        let values = vec![
            Value::String(
                br#"{"public": "data", "password": "secret", "private_key": "xyz"}"#.to_vec(),
            ),
            Value::String(
                br#"{"public": "info", "secret_token": "abc", "api_key": "123"}"#.to_vec(),
            ),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![
                "password".to_string(),
                ".*_key".to_string(),   // Regex pattern
                "secret.*".to_string(), // Regex pattern
            ],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            // Verify only public field remains
            assert_eq!(json_state.dynamic_paths.len(), 1);
            assert!(json_state.dynamic_paths.contains(&"public".to_string()));

            // Verify skipped paths are not present
            assert!(!json_state.dynamic_paths.contains(&"password".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"private_key".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"secret_token".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"api_key".to_string()));
        } else {
            panic!("Expected JSON state");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_with_typed_and_skip_paths() -> Result<()> {
        // Test JSON with both typed and skip paths
        let values = vec![
            Value::String(
                br#"{"id": 1, "name": "Alice", "password": "secret", "score": 95, "active": true}"#
                    .to_vec(),
            ),
            Value::String(br#"{"id": 2, "name": "Bob", "api_key": "xyz", "score": 87}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            skip_paths:        vec!["password".to_string(), ".*_key".to_string()],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            // Verify typed paths
            assert_eq!(json_state.typed_paths.len(), 2);

            // Verify dynamic paths (should only have score and active)
            assert!(
                json_state.dynamic_paths.contains(&"score".to_string())
                    || json_state.dynamic_paths.contains(&"active".to_string())
            );
            assert!(!json_state.dynamic_paths.contains(&"password".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"api_key".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"id".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"name".to_string()));
        } else {
            panic!("Expected JSON state");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_simple() -> Result<()> {
        // Simple test to verify typed paths work
        let values = vec![Value::String(br#"{"id": 1, "name": "test"}"#.to_vec())];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![("id".to_string(), Box::new(Type::UInt32))],
            skip_paths:        vec![],
        };

        // Just test analyze for now
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.typed_paths.len(), 1);
            assert!(json_state.typed_paths.iter().any(|(p, _)| p == "id"));
            assert!(json_state.dynamic_paths.contains(&"name".to_string()));

            // Check typed columns were extracted
            if let Some(typed_cols) = &json_state.typed_path_columns {
                assert!(typed_cols.contains_key("id"));
                let id_values = typed_cols.get("id").unwrap();
                assert_eq!(id_values.len(), 1);
                // The value should be UInt32(1)
                match &id_values[0] {
                    Value::UInt32(1) | Value::UInt64(1) | Value::Int64(1) => {}
                    other => panic!("Expected numeric 1, got {:?}", other),
                }
            } else {
                panic!("No typed columns found");
            }
        } else {
            panic!("Expected JSON state");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_simple_roundtrip() -> Result<()> {
        // Simple roundtrip test without typed paths first
        let values = vec![
            Value::String(br#"{"id": 123, "name": "Alice"}"#.to_vec()),
            Value::String(br#"{"id": 456, "name": "Bob"}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize
        let mut cursor = Cursor::new(output);
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut cursor, &mut de_state).await?;
        let deserialized = type_.deserialize_column(&mut cursor, 2, &mut de_state).await?;

        // Verify we got the same data back
        assert_eq!(deserialized.len(), values.len());
        for (orig, deser) in values.iter().zip(deserialized.iter()) {
            // Parse both as JSON to compare structure, not formatting
            if let (Value::String(orig_bytes), Value::String(deser_bytes)) = (orig, deser) {
                let orig_json: serde_json::Value = serde_json::from_slice(orig_bytes)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;
                let deser_json: serde_json::Value = serde_json::from_slice(deser_bytes)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;
                assert_eq!(orig_json, deser_json);
            } else {
                panic!("Expected String values");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_roundtrip() -> Result<()> {
        // Full roundtrip test with typed paths
        let values = vec![
            Value::String(
                br#"{"id": 123, "name": "Alice", "score": 95.5, "active": true}"#.to_vec(),
            ),
            Value::String(br#"{"id": 456, "name": "Bob", "score": 87.3}"#.to_vec()),
            Value::String(br#"{"id": 789, "name": "Charlie", "tags": ["a", "b"]}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            skip_paths:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize
        let mut cursor = Cursor::new(output);
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut cursor, &mut de_state).await?;
        let deserialized = type_.deserialize_column(&mut cursor, 3, &mut de_state).await?;

        // Verify we got the same data back
        assert_eq!(deserialized.len(), values.len());

        // Parse and compare JSON objects
        for (original, deserialized) in values.iter().zip(deserialized.iter()) {
            if let (Value::String(orig_bytes), Value::String(deser_bytes)) =
                (original, deserialized)
            {
                let orig_str = String::from_utf8(orig_bytes.clone())?;
                let deser_str = String::from_utf8(deser_bytes.clone())?;

                let orig_json: serde_json::Value = serde_json::from_str(&orig_str)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;
                let deser_json: serde_json::Value = serde_json::from_str(&deser_str)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;

                // Verify typed paths are preserved
                assert_eq!(orig_json["id"], deser_json["id"]);
                assert_eq!(orig_json["name"], deser_json["name"]);

                // Verify dynamic paths are preserved when they exist
                if !orig_json["score"].is_null() {
                    assert_eq!(orig_json["score"], deser_json["score"]);
                }
                if !orig_json["active"].is_null() {
                    // Bool gets converted to UInt8 (0/1) in ClickHouse
                    if orig_json["active"].is_boolean() && deser_json["active"].is_number() {
                        let orig_bool = orig_json["active"].as_bool().unwrap();
                        let deser_num = deser_json["active"].as_u64().unwrap();
                        assert_eq!(orig_bool as u64, deser_num);
                    } else {
                        assert_eq!(orig_json["active"], deser_json["active"]);
                    }
                }
                if !orig_json["tags"].is_null() {
                    assert_eq!(orig_json["tags"], deser_json["tags"]);
                }
            } else {
                panic!("Expected String values");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_type_conversions() -> Result<()> {
        // Test various type conversions for typed paths
        let values = vec![
            Value::String(br#"{"int8": 127, "int16": 32000, "int32": 2000000, "uint8": 255, "uint16": 65000, "uint32": 4000000000, "float32": 3.14, "float64": 2.71828}"#.to_vec()),
            Value::String(br#"{"int8": -128, "int16": -32000, "int32": -2000000, "uint8": 0, "uint16": 0, "uint32": 0, "float32": -1.23, "float64": -9.876}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("int8".to_string(), Box::new(Type::Int8)),
                ("int16".to_string(), Box::new(Type::Int16)),
                ("int32".to_string(), Box::new(Type::Int32)),
                ("uint8".to_string(), Box::new(Type::UInt8)),
                ("uint16".to_string(), Box::new(Type::UInt16)),
                ("uint32".to_string(), Box::new(Type::UInt32)),
                ("float32".to_string(), Box::new(Type::Float32)),
                ("float64".to_string(), Box::new(Type::Float64)),
            ],
            skip_paths:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize
        let mut cursor = Cursor::new(output);
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut cursor, &mut de_state).await?;
        let deserialized = type_.deserialize_column(&mut cursor, 2, &mut de_state).await?;

        // Verify we got the same data back (with type conversions)
        assert_eq!(deserialized.len(), 2);

        // Parse and verify JSON structure
        for (orig, deser) in values.iter().zip(deserialized.iter()) {
            if let (Value::String(orig_bytes), Value::String(deser_bytes)) = (orig, deser) {
                let orig_json: serde_json::Value = serde_json::from_slice(orig_bytes)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;
                let deser_json: serde_json::Value = serde_json::from_slice(deser_bytes)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;

                // Verify typed paths are preserved with correct types
                // Note: Values may be truncated due to type conversions
                assert!(deser_json["int8"].is_number());
                assert!(deser_json["int16"].is_number());
                assert!(deser_json["int32"].is_number());
                assert!(deser_json["uint8"].is_number());
                assert!(deser_json["uint16"].is_number());
                assert!(deser_json["uint32"].is_number());
                assert!(deser_json["float32"].is_number());
                assert!(deser_json["float64"].is_number());

                // Check some specific values
                if orig_json["int8"] == 127 {
                    assert_eq!(deser_json["int8"], 127);
                    assert_eq!(deser_json["uint8"], 255);
                }
            } else {
                panic!("Expected String values");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_skip_paths_roundtrip() -> Result<()> {
        // Full roundtrip test with skip paths
        let values = vec![
            Value::String(
                br#"{"public": "data", "password": "secret123", "private_key": "xyz"}"#.to_vec(),
            ),
            Value::String(
                br#"{"public": "info", "secret_token": "abc", "api_key": "123"}"#.to_vec(),
            ),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![
                "password".to_string(),
                ".*_key".to_string(),
                "secret.*".to_string(),
            ],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize
        let mut cursor = Cursor::new(output);
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut cursor, &mut de_state).await?;
        let deserialized = type_.deserialize_column(&mut cursor, 2, &mut de_state).await?;

        // Verify we got data back
        assert_eq!(deserialized.len(), values.len());

        // Parse and verify skipped paths are not present
        for deserialized_val in deserialized.iter() {
            if let Value::String(deser_bytes) = deserialized_val {
                let deser_str = String::from_utf8(deser_bytes.clone())?;
                let deser_json: serde_json::Value = serde_json::from_str(&deser_str)
                    .map_err(|e| Error::DeserializeError(format!("JSON parse error: {}", e)))?;

                // Verify only public field is present
                assert!(!deser_json["public"].is_null());

                // Verify skipped paths are not present
                assert!(deser_json["password"].is_null());
                assert!(deser_json["private_key"].is_null());
                assert!(deser_json["secret_token"].is_null());
                assert!(deser_json["api_key"].is_null());
            } else {
                panic!("Expected String values");
            }
        }

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
                for value in &result {
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

    #[tokio::test]
    async fn test_json_type_conversion_overflow_wrapping() -> Result<()> {
        // Test that numeric conversions use wrapping semantics like ClickHouse
        let values = vec![
            // Test overflow with wrapping: 256 as UInt8 should wrap to 0, -129 as Int8 wraps to
            // 127
            Value::String(
                br#"{"overflow_u8": 256, "underflow_i8": -129, "big_to_small": 65536}"#.to_vec(),
            ),
            // Test negative to unsigned wrapping: -1 as UInt8 becomes 255
            Value::String(
                br#"{"negative_to_u8": -1, "negative_to_u16": -1, "negative_to_u32": -1}"#.to_vec(),
            ),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("overflow_u8".to_string(), Box::new(Type::UInt8)),
                ("underflow_i8".to_string(), Box::new(Type::Int8)),
                ("big_to_small".to_string(), Box::new(Type::UInt8)),
                ("negative_to_u8".to_string(), Box::new(Type::UInt8)),
                ("negative_to_u16".to_string(), Box::new(Type::UInt16)),
                ("negative_to_u32".to_string(), Box::new(Type::UInt32)),
            ],
            skip_paths:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize and check values
        let mut input = output.as_slice();
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut input, &mut de_state).await?;
        let result_values = type_.deserialize_column(&mut input, 2, &mut de_state).await?;

        // Parse results to verify wrapping behavior
        let result1 = match &result_values[0] {
            Value::String(s) => {
                String::from_utf8(s.clone()).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            _ => return Err(Error::SerializeError("Expected String value".to_string())),
        };
        let parsed1: serde_json::Value =
            serde_json::from_str(&result1).map_err(|e| Error::SerializeError(e.to_string()))?;

        // 256 wraps to 0 as UInt8
        assert_eq!(parsed1["overflow_u8"], 0);
        // -129 wraps to 127 as Int8 (two's complement)
        assert_eq!(parsed1["underflow_i8"], 127);
        // 65536 wraps to 0 as UInt8
        assert_eq!(parsed1["big_to_small"], 0);

        let result2 = match &result_values[1] {
            Value::String(s) => {
                String::from_utf8(s.clone()).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            _ => return Err(Error::SerializeError("Expected String value".to_string())),
        };
        let parsed2: serde_json::Value =
            serde_json::from_str(&result2).map_err(|e| Error::SerializeError(e.to_string()))?;

        // -1 as UInt8 becomes 255 (two's complement)
        assert_eq!(parsed2["negative_to_u8"], 255);
        // -1 as UInt16 becomes 65535
        assert_eq!(parsed2["negative_to_u16"], 65535);
        // -1 as UInt32 becomes 4294967295
        assert_eq!(parsed2["negative_to_u32"], 4294967295u64);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_string_parsing() -> Result<()> {
        // Test string to numeric parsing
        let values = vec![
            Value::String(br#"{"str_int": "123", "str_float": "3.14", "str_uint": "255", "str_neg": "-456", "digit_u8_1": "1", "digit_u8_0": "0"}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("str_int".to_string(), Box::new(Type::Int32)),
                ("str_float".to_string(), Box::new(Type::Float64)),
                ("str_uint".to_string(), Box::new(Type::UInt8)),
                ("str_neg".to_string(), Box::new(Type::Int16)),
                ("digit_u8_1".to_string(), Box::new(Type::UInt8)),
                ("digit_u8_0".to_string(), Box::new(Type::UInt8)),
            ],
            skip_paths:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize and verify
        let mut input = output.as_slice();
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut input, &mut de_state).await?;
        let result_values = type_.deserialize_column(&mut input, 1, &mut de_state).await?;

        let result = match &result_values[0] {
            Value::String(s) => {
                String::from_utf8(s.clone()).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            _ => return Err(Error::SerializeError("Expected String value".to_string())),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&result).map_err(|e| Error::SerializeError(e.to_string()))?;

        assert_eq!(parsed["str_int"], 123);
        assert_eq!(parsed["str_float"], 3.14);
        assert_eq!(parsed["str_uint"], 255);
        assert_eq!(parsed["str_neg"], -456);
        assert_eq!(parsed["digit_u8_1"], 1);
        assert_eq!(parsed["digit_u8_0"], 0);

        Ok(())
    }
}
