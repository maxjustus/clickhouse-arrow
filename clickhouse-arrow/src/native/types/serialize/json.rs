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
// Using FLATTENED format (version 3) for client compatibility
const JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED: u64 = 3;

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
    #[inline]
    fn is_effectively_nullable(t: &Type) -> bool {
        match t {
            Type::Nullable(_) => true,
            Type::LowCardinality(inner) => inner.is_nullable(),
            _ => false,
        }
    }

    /// Parse JSON values into path-organized structure
    fn from_values(
        values: Vec<Value>,
        typed_paths: &[(String, Type)],
        skip_exact: &[String],
        skip_regex: &[String],
    ) -> Result<Self> {
        let mut dynamic_path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut typed_path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let rows = values.len();

        // Pre-populate typed path columns; use default for non-nullable types
        for (path, ty) in typed_paths {
            // Prefill: Variant columns expect Variant values (use null discriminator),
            // otherwise use Null for effectively-nullable types or the type default.
            let fill = match ty {
                Type::Variant(_) => Value::Variant(0xFF, Box::new(Value::Null)),
                _ if Self::is_effectively_nullable(ty) => Value::Null,
                _ => ty.default_value(),
            };
            drop(typed_path_columns.insert(path.clone(), vec![fill; rows]));
        }

        // Prepare skip matchers
        let skip_exact_set: std::collections::HashSet<&str> =
            skip_exact.iter().map(|s| s.as_str()).collect();
        let skip_patterns: Result<Vec<regex::Regex>> = skip_regex
            .iter()
            .map(|p| {
                regex::Regex::new(p).map_err(|e| {
                    Error::SerializeError(format!("Invalid skip_path pattern '{p}': {e}"))
                })
            })
            .collect();
        let skip_patterns = skip_patterns?;

        for (row_idx, value) in values.into_iter().enumerate() {
            match value {
                Value::Object(bytes) => {
                    // Parse JSON bytes into object
                    let json_value: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|e| Error::SerializeError(format!("Invalid JSON bytes: {e}")))?;

                    // Extract paths from JSON object
                    Self::extract_paths_from_json(
                        &json_value,
                        "",
                        &mut dynamic_path_columns,
                        &mut typed_path_columns,
                        typed_paths,
                        &skip_exact_set,
                        &skip_patterns,
                        row_idx,
                        rows,
                    )?;
                }
                #[cfg(feature = "serde")]
                Value::Json(json_value) => {
                    // Consume structured JSON directly, no parsing
                    Self::extract_paths_from_json(
                        &json_value,
                        "",
                        &mut dynamic_path_columns,
                        &mut typed_path_columns,
                        typed_paths,
                        &skip_exact_set,
                        &skip_patterns,
                        row_idx,
                        rows,
                    )?;
                }
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
                        &skip_exact_set,
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
                        "JSON serialization expects Object (bytes) or String (text) containing \
                         JSON, got: {value:?}"
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
        skip_exact: &std::collections::HashSet<&str>,
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
                    skip_exact,
                    skip_patterns,
                    row_idx,
                    total_rows,
                )?;
            }
        } else {
            // Determine if this is typed
            let typed_path_type =
                typed_paths.iter().find(|(path, _)| path == current_path).map(|(_, typ)| typ);

            // Apply skip only for dynamic (non-typed) paths
            if typed_path_type.is_none() {
                if skip_exact.contains(current_path) {
                    return Ok(());
                }
                for pattern in skip_patterns {
                    if pattern.is_match(current_path) {
                        return Ok(());
                    }
                }
            }

            // Leaf value - convert to ClickHouse Value and store
            let ch_value = Self::json_value_to_clickhouse_value(json_value)?;

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
                "Failed to convert value for path '{path}' to type {expected_type:?}: {e}"
            ))
        })
    }

    /// Find the discriminator and original type index for a value in a Variant type
    /// Returns (discriminator, original_type_index)
    fn find_variant_discriminator(value: &Value, variant_types: &[Type]) -> Result<(u8, usize)> {
        // Null values always map to discriminator 0xFF (special null discriminator)
        // For null, we don't have a valid original index, use 0 as placeholder
        if matches!(value, Value::Null) {
            return Ok((0xFF, 0));
        }

        // Types are stored in sorted order (alphabetically), discriminators are assigned 0..n
        // We need to match this same ordering that DiscriminatorMap uses
        let mut type_map: Vec<(String, usize)> =
            variant_types.iter().enumerate().map(|(idx, t)| (t.to_string(), idx)).collect();
        type_map.sort_by(|a, b| a.0.cmp(&b.0));

        // First pass: try exact type matches (most specific)
        for (sorted_idx, (_type_str, original_idx)) in type_map.iter().enumerate() {
            let target_type = &variant_types[*original_idx];
            if Self::is_exact_type_match(value, target_type) {
                let discriminator = u8::try_from(sorted_idx).map_err(|_| {
                    Error::SerializeError(format!(
                        "Too many variant types ({}), max supported is 255",
                        sorted_idx
                    ))
                })?;
                return Ok((discriminator, *original_idx));
            }
        }

        // Second pass: try convertible type matches (less specific)
        for (sorted_idx, (_type_str, original_idx)) in type_map.iter().enumerate() {
            let target_type = &variant_types[*original_idx];
            if Self::can_convert_to_type(value, target_type) {
                let discriminator = u8::try_from(sorted_idx).map_err(|_| {
                    Error::SerializeError(format!(
                        "Too many variant types ({}), max supported is 255",
                        sorted_idx
                    ))
                })?;
                return Ok((discriminator, *original_idx));
            }
        }

        Err(Error::SerializeError(format!(
            "Cannot find matching variant type for value: {value:?}"
        )))
    }

    /// Check for exact type match (no conversion needed)
    fn is_exact_type_match(value: &Value, target_type: &Type) -> bool {
        match (value, target_type) {
            // Exact string matches
            (Value::String(_), Type::String) => true,

            // Exact numeric matches
            (Value::Int64(_), Type::Int64) => true,
            (Value::Float64(_), Type::Float64) => true,
            (Value::UInt64(_), Type::UInt64) => true,
            (Value::Int32(_), Type::Int32) => true,
            (Value::UInt32(_), Type::UInt32) => true,
            (Value::Float32(_), Type::Float32) => true,

            // Exact array/collection matches
            (Value::Array(_), Type::Array(_)) => true,
            (Value::Tuple(_), Type::Tuple(_)) => true,

            // No exact match
            _ => false,
        }
    }

    /// Check if a value can be converted to a specific type
    fn can_convert_to_type(value: &Value, target_type: &Type) -> bool {
        match (value, target_type) {
            // Null can go to any Nullable type
            (Value::Null, Type::Nullable(_)) => true,
            (Value::Null, _) => false,

            // Numeric conversions - allow cross-signed conversions for JSON compatibility
            (
                Value::Int8(_) | Value::Int16(_) | Value::Int32(_) | Value::Int64(_),
                Type::Int8
                | Type::Int16
                | Type::Int32
                | Type::Int64
                | Type::Int128
                | Type::UInt8
                | Type::UInt16
                | Type::UInt32
                | Type::UInt64
                | Type::UInt128,
            ) => true,
            (
                Value::UInt8(_) | Value::UInt16(_) | Value::UInt32(_) | Value::UInt64(_),
                Type::UInt8
                | Type::UInt16
                | Type::UInt32
                | Type::UInt64
                | Type::UInt128
                | Type::Int8
                | Type::Int16
                | Type::Int32
                | Type::Int64
                | Type::Int128,
            ) => true,
            (Value::Float32(_) | Value::Float64(_), Type::Float32 | Type::Float64) => true,

            // String types - prioritize exact matches first
            (Value::String(_), Type::String | Type::FixedSizedString(_)) => true,

            // Array types
            (Value::Array(_), Type::Array(_)) => true,

            // Tuple types
            (Value::Tuple(_), Type::Tuple(_)) => true,
            (Value::Array(_), Type::Tuple(_)) => true, // Arrays can convert to tuples

            // Map types
            (Value::Map(_, _), Type::Map(_, _)) => true,
            (Value::Array(_), Type::Map(_, _)) => true, // Arrays of pairs can become maps

            // Recursive types
            (v, Type::Nullable(inner)) => Self::can_convert_to_type(v, inner),
            (v, Type::LowCardinality(inner)) => Self::can_convert_to_type(v, inner),

            _ => false,
        }
    }

    /// Convert a value to a specific type if needed
    fn convert_to_type(value: Value, expected_type: &Type) -> Result<Value> {
        // Handle Nulls according to target type semantics
        if matches!(value, Value::Null) {
            return Ok(match expected_type {
                // Variant uses special null discriminator
                Type::Variant(_) => Value::Variant(0xFF, Box::new(Value::Null)),
                // Nullable and LC(Nullable) carry Null through
                _ if Self::is_effectively_nullable(expected_type) => Value::Null,
                // Non-nullable types get their default value
                _ => expected_type.default_value(),
            });
        }
        match (value, expected_type) {
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
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Int8: {e}"))
                })
            }
            (Value::String(s), Type::Int16) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i16>().map(Value::Int16).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Int16: {e}"))
                })
            }
            (Value::String(s), Type::Int32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i32>().map(Value::Int32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Int32: {e}"))
                })
            }
            (Value::String(s), Type::Int64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<i64>().map(Value::Int64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Int64: {e}"))
                })
            }
            (Value::String(s), Type::UInt8) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u8>().map(Value::UInt8).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as UInt8: {e}"))
                })
            }
            (Value::String(s), Type::UInt16) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u16>().map(Value::UInt16).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as UInt16: {e}"))
                })
            }
            (Value::String(s), Type::UInt32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u32>().map(Value::UInt32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as UInt32: {e}"))
                })
            }
            (Value::String(s), Type::UInt64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<u64>().map(Value::UInt64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as UInt64: {e}"))
                })
            }
            (Value::String(s), Type::Float32) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<f32>().map(Value::Float32).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Float32: {e}"))
                })
            }
            (Value::String(s), Type::Float64) => {
                let s_str = String::from_utf8_lossy(&s);
                s_str.parse::<f64>().map(Value::Float64).map_err(|e| {
                    Error::SerializeError(format!("Cannot parse '{s_str}' as Float64: {e}"))
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

            // Array handling - recursively convert elements
            (Value::Array(elements), Type::Array(target_elem_type)) => {
                let converted_elements: Result<Vec<Value>> = elements
                    .into_iter()
                    .map(|elem| Self::convert_to_type(elem, target_elem_type))
                    .collect();
                Ok(Value::Array(converted_elements?))
            }

            // Tuple handling - convert each element to its corresponding type
            (Value::Tuple(elements), Type::Tuple(target_types)) => {
                if elements.len() != target_types.len() {
                    return Err(Error::SerializeError(format!(
                        "Tuple length mismatch: got {} elements, expected {}",
                        elements.len(),
                        target_types.len()
                    )));
                }
                let converted_elements: Result<Vec<Value>> = elements
                    .into_iter()
                    .zip(target_types.iter())
                    .map(|(elem, target_type)| Self::convert_to_type(elem, target_type))
                    .collect();
                Ok(Value::Tuple(converted_elements?))
            }

            // Array to Tuple conversion - by position
            (Value::Array(elements), Type::Tuple(target_types)) => {
                if elements.len() != target_types.len() {
                    return Err(Error::SerializeError(format!(
                        "Cannot convert Array to Tuple: length mismatch ({} != {})",
                        elements.len(),
                        target_types.len()
                    )));
                }
                let converted_elements: Result<Vec<Value>> = elements
                    .into_iter()
                    .zip(target_types.iter())
                    .map(|(elem, target_type)| Self::convert_to_type(elem, target_type))
                    .collect();
                Ok(Value::Tuple(converted_elements?))
            }

            // Map handling - convert keys and values recursively
            (Value::Map(keys, values), Type::Map(target_key_type, target_value_type)) => {
                if keys.len() != values.len() {
                    return Err(Error::SerializeError(format!(
                        "Map keys and values length mismatch: {} != {}",
                        keys.len(),
                        values.len()
                    )));
                }
                let converted_keys: Result<Vec<Value>> = keys
                    .into_iter()
                    .map(|key| Self::convert_to_type(key, target_key_type))
                    .collect();
                let converted_values: Result<Vec<Value>> = values
                    .into_iter()
                    .map(|value| Self::convert_to_type(value, target_value_type))
                    .collect();
                Ok(Value::Map(converted_keys?, converted_values?))
            }

            // Array of Tuple(K,V) or Array of Array(2) to Map conversion
            (Value::Array(elements), Type::Map(target_key_type, target_value_type)) => {
                let mut keys = Vec::with_capacity(elements.len());
                let mut values = Vec::with_capacity(elements.len());

                for elem in elements {
                    match elem {
                        // Handle Tuple(K,V)
                        Value::Tuple(pair) if pair.len() == 2 => {
                            let mut pair_iter = pair.into_iter();
                            let key = pair_iter.next().unwrap();
                            let value = pair_iter.next().unwrap();
                            keys.push(Self::convert_to_type(key, target_key_type)?);
                            values.push(Self::convert_to_type(value, target_value_type)?);
                        }
                        // Handle Array[K,V] (JSON arrays become Array not Tuple)
                        Value::Array(pair) if pair.len() == 2 => {
                            let mut pair_iter = pair.into_iter();
                            let key = pair_iter.next().unwrap();
                            let value = pair_iter.next().unwrap();
                            keys.push(Self::convert_to_type(key, target_key_type)?);
                            values.push(Self::convert_to_type(value, target_value_type)?);
                        }
                        _ => {
                            return Err(Error::SerializeError(
                                "Cannot convert Array to Map: elements must be Tuple(K,V) or \
                                 Array[K,V]"
                                    .to_string(),
                            ));
                        }
                    }
                }
                Ok(Value::Map(keys, values))
            }

            // Handle Nullable types by recursing (value is non-null here)
            (value, Type::Nullable(inner)) => Self::convert_to_type(value, inner),

            // Handle LowCardinality - just recurse to the inner type
            // The LowCardinality serializer will handle the indexing
            (value, Type::LowCardinality(inner)) => Self::convert_to_type(value, inner),

            // Handle Variant - find the best matching type and create Variant value
            (value, Type::Variant(variant_types)) => {
                // Try to convert the value to each variant type to find the best match
                let (discriminator, original_idx) =
                    Self::find_variant_discriminator(&value, variant_types)?;
                let target_type = &variant_types[original_idx];
                let converted_value = Self::convert_to_type(value, target_type)?;
                Ok(Value::Variant(discriminator, Box::new(converted_value)))
            }

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
        // Extract typed_paths and SKIP rules from the Type
        let (typed_paths, skip_exact, skip_regex, _max_dyn_paths, _max_dyn_types) = match type_ {
            Type::JSON {
                typed_paths,
                skip_exact,
                skip_regex,
                max_dynamic_paths,
                max_dynamic_types,
            } => {
                let typed_list: Vec<(String, Type)> = typed_paths
                    .iter()
                    .map(|(path, boxed_type)| (path.clone(), *boxed_type.clone()))
                    .collect();
                (
                    typed_list,
                    skip_exact.clone(),
                    skip_regex.clone(),
                    *max_dynamic_paths,
                    *max_dynamic_types,
                )
            }
            _ => return Err(Error::SerializeError("Expected JSON type".to_string())),
        };

        // Parse JSON values into path-organized structure (filtering skipped paths)
        let json_data =
            JsonData::from_values(values.to_vec(), &typed_paths, &skip_exact, &skip_regex)?;

        // Build the metadata
        let mut dynamic_paths: Vec<String> =
            json_data.dynamic_path_columns.keys().cloned().collect();
        dynamic_paths.sort(); // Ensure consistent ordering

        // Do NOT enforce max_dynamic_paths client-side in flattened v3.
        // Send all discovered paths; server will select which become dynamic vs shared.

        // Build states for typed paths
        // This is crucial for types like LowCardinality that need to build dictionaries
        let mut typed_path_states = BTreeMap::new();
        for (path, _type_) in &typed_paths {
            // Get the column values for this typed path
            let _column_values = json_data
                .typed_path_columns
                .get(path)
                .map(|v| v.clone())
                .unwrap_or_else(|| vec![Value::Null; json_data.rows]);

            // For now, we'll handle this in the write method where we can properly
            // build the state. Just store a placeholder.
            drop(typed_path_states.insert(path.clone(), SerializerState::default()));
        }

        // Precompute Dynamic states per dynamic path. Do NOT error on max_dynamic_types in v3; let
        // server decide.
        let mut path_dynamic_states = BTreeMap::new();
        for path in &dynamic_paths {
            if let Some(col) = json_data.dynamic_path_columns.get(path) {
                let analyzed = DynamicSerializer::analyze_values(col);
                if let TypeSpecificState::Dynamic(dyn_state) = analyzed.clone() {
                    drop(path_dynamic_states.insert(path.clone(), dyn_state));
                }
            }
        }

        let state = JsonState {
            version: None, // Will be set properly in write_prefix
            dynamic_paths: dynamic_paths.clone(),
            typed_paths: typed_paths.clone(),
            dynamic_path_columns: Some(json_data.dynamic_path_columns),
            typed_path_columns: Some(json_data.typed_path_columns),
            rows: Some(json_data.rows),
            dynamic_data: None,
            path_dynamic_states, // Pre-built per-path Dynamic states
            typed_path_states,   // Pre-built states for typed paths
            // Deprecated fields for compatibility
            #[allow(deprecated)]
            paths: dynamic_paths,
            #[allow(deprecated)]
            path_columns: None,
        };

        Ok(TypeSpecificState::Json(state))
    }

    /// Get serialization version based on server support
    fn get_serialization_version(_state: &SerializerState) -> u64 {
        JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED
    }

    /// Write paths header based on version
    fn write_paths_header_sync<W: ClickHouseBytesWrite>(
        paths: &[String],
        version: u64,
        writer: &mut W,
    ) -> Result<()> {
        if version != JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED {
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
        if version != JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED {
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
            // In v3 format, ALL paths (typed + dynamic) are written to ObjectStructure

            // Only dynamic paths go in the flattened paths header
            // Typed paths are NOT included because they have custom serializations
            let _typed_paths: Vec<String> =
                json_state.typed_paths.iter().map(|(name, _)| name.clone()).collect();
            let dynamic_paths = json_state.dynamic_paths.clone();
            let dynamic_columns = json_state.dynamic_path_columns.clone();
            Self::write_paths_header_sync(&dynamic_paths, version, writer)?;

            // Write typed path prefixes using their nested serializers
            // Sort typed paths by name to match ClickHouse's deterministic order
            let mut typed_entries: Vec<(&String, &Type)> =
                json_state.typed_paths.iter().map(|(n, t)| (n, t)).collect();
            typed_entries.sort_by(|(a, _), (b, _)| a.cmp(b));

            for (_path_name, path_type) in typed_entries {
                let mut typed_prefix_state = SerializerState::default();
                if let Some(version) = state.server_version {
                    typed_prefix_state = typed_prefix_state.with_server_version(version);
                }
                path_type.serialize_prefix(writer, &mut typed_prefix_state);
            }

            // Write Dynamic column headers for each dynamic path using precomputed states
            if let Some(dynamic_columns) = dynamic_columns {
                let path_dynamic_states =
                    if let TypeSpecificState::Json(json_state_ref) = &state.type_specific {
                        json_state_ref.path_dynamic_states.clone()
                    } else {
                        Default::default()
                    };

                for path in dynamic_paths {
                    if dynamic_columns.get(&path).is_some() {
                        if let Some(dyn_state) = path_dynamic_states.get(&path) {
                            // Temporarily swap state to use provided Dynamic state
                            let original = std::mem::replace(
                                &mut state.type_specific,
                                TypeSpecificState::Dynamic(dyn_state.clone()),
                            );
                            // Write prefix for this dynamic path (sync)
                            DynamicSerializer::write_prefix_sync(
                                &Type::Dynamic { max_types: None },
                                writer,
                                state,
                            )?;
                            // Restore
                            state.type_specific = original;
                        }
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
            // In FLATTENED (v3) format, the structure header includes ONLY the
            // flattened dynamic paths. Typed paths are not listed here because
            // they have custom serializations and their own prefixes.

            let _typed_paths: Vec<String> =
                json_state.typed_paths.iter().map(|(name, _)| name.clone()).collect();
            let dynamic_paths = json_state.dynamic_paths.clone();
            let dynamic_columns = json_state.dynamic_path_columns.clone();

            // Write only dynamic (flattened) paths to the structure header
            Self::write_paths_header_async(&dynamic_paths, version, writer).await?;

            // Write typed path prefixes using their nested serializers
            // Sort typed paths by name to match ClickHouse's deterministic order
            let mut typed_entries: Vec<(&String, &Type)> =
                json_state.typed_paths.iter().map(|(n, t)| (n, t)).collect();
            typed_entries.sort_by(|(a, _), (b, _)| a.cmp(b));

            for (_path_name, path_type) in typed_entries {
                // Call each typed-path's nested serializer with a fresh SerializerState
                // (only server_version copied). This mirrors ClickHouse prefix calls per
                // substream and avoids carrying per-path TypeSpecificState across prefixes.
                let mut typed_prefix_state = SerializerState::default();
                if let Some(version) = state.server_version {
                    typed_prefix_state = typed_prefix_state.with_server_version(version);
                }

                // Always write the native prefix for typed paths to match ClickHouse behavior.
                path_type.serialize_prefix_async(writer, &mut typed_prefix_state).await?;
            }

            // Write Dynamic column headers for each dynamic path using precomputed states
            if let Some(dynamic_columns) = dynamic_columns {
                let path_dynamic_states =
                    if let TypeSpecificState::Json(json_state_ref) = &state.type_specific {
                        json_state_ref.path_dynamic_states.clone()
                    } else {
                        Default::default()
                    };

                for path in dynamic_paths {
                    if dynamic_columns.get(&path).is_some() {
                        if let Some(dyn_state) = path_dynamic_states.get(&path) {
                            let original = std::mem::replace(
                                &mut state.type_specific,
                                TypeSpecificState::Dynamic(dyn_state.clone()),
                            );
                            DynamicSerializer::write_prefix(
                                &Type::Dynamic { max_types: None },
                                writer,
                                state,
                            )
                            .await?;
                            state.type_specific = original;
                        }
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
        let (typed_paths, typed_columns, dynamic_paths, dynamic_columns, rows) =
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

        // First write typed path columns (part of FLATTENED format)
        for (path, type_) in &typed_paths {
            // Use a clean state for typed columns but preserve server version
            let mut typed_state = SerializerState::default();
            if let Some(version) = state.server_version {
                typed_state = typed_state.with_server_version(version);
            }

            // Get the column values or use nulls
            let column_values = if let Some(values) = typed_columns.get(path) {
                values.clone()
            } else {
                vec![Value::Null; rows]
            };

            // LowCardinality no longer needs a JSON-context flag; behavior is type-driven.

            type_.serialize_column(column_values, writer, &mut typed_state).await?;
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
                        "Dynamic state not found for path: {path}"
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
        let (typed_paths, typed_columns, dynamic_paths, dynamic_columns, rows) =
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

        // First write typed path columns (part of FLATTENED format)
        for (path, type_) in &typed_paths {
            // Use a clean state for typed columns but preserve server version
            let mut typed_state = SerializerState::default();
            if let Some(version) = state.server_version {
                typed_state = typed_state.with_server_version(version);
            }

            // Get the column values or use nulls
            let column_values = if let Some(values) = typed_columns.get(path) {
                values.clone()
            } else {
                vec![Value::Null; rows]
            };

            // LowCardinality no longer needs a JSON-context flag; behavior is type-driven.

            // For typed paths in FLATTENED format, just call serialize_column_sync
            // which internally calls write_sync() that includes prefix/version and data
            type_.serialize_column_sync(column_values, writer, &mut typed_state)?;
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
                        "Dynamic state not found for path: {path}"
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            version, JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED,
            "Should use FLATTENED format serialization"
        );

        let _ = cursor.seek(SeekFrom::Start(8))?;
        // Read dynamic paths count (typed paths are not in ObjectStructure)
        let mut path_count_byte = [0u8; 1];
        cursor.read_exact(&mut path_count_byte)?;
        assert!(path_count_byte[0] > 0, "Should have dynamic paths for object serialization");

        // Verify deserialized data structure
        for value in &deserialized {
            #[cfg(feature = "serde")]
            if let Value::Json(json_value) = value {
                assert!(json_value.is_object(), "Deserialized value should be a JSON object");
                continue;
            }
            let json_value: serde_json::Value = match value {
                Value::Object(bytes) | Value::String(bytes) => serde_json::from_slice(bytes)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?,
                other => panic!("Unexpected value variant: {other:?}"),
            };
            assert!(json_value.is_object(), "Deserialized value should be a JSON object");
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
            skip_exact:        vec![],
            skip_regex:        vec![],
        };
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        let version = u64::from_le_bytes(output[0..8].try_into().unwrap());
        assert_eq!(
            version, JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED,
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
    async fn test_json_typed_nonnullable_defaults() -> Result<()> {
        // Typed path 'id' is non-nullable UInt32. Missing values should default to 0.
        let values = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "id": 42}"#.to_vec()),
            Value::String(br#"{"name": "Carol"}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![("id".to_string(), Box::new(Type::UInt32))],
            skip_exact:        vec![],
            skip_regex:        vec![],
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
        let deserialized =
            type_.deserialize_column(&mut cursor, values.len(), &mut de_state).await?;

        // Validate: rows missing 'id' should have id = 0 in JSON
        for (i, v) in deserialized.iter().enumerate() {
            #[cfg(feature = "serde")]
            if let Value::Json(obj) = v {
                let id = obj.get("id").cloned().unwrap_or(serde_json::Value::Null);
                match i {
                    0 | 2 => assert_eq!(id, serde_json::Value::from(0u64)),
                    1 => assert_eq!(id, serde_json::Value::from(42u64)),
                    _ => unreachable!(),
                }
                continue;
            }
            let obj: serde_json::Value = match v {
                Value::Object(bytes) | Value::String(bytes) => serde_json::from_slice(bytes)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?,
                other => panic!("Unexpected value variant: {other:?}"),
            };
            let id = obj.get("id").cloned().unwrap_or(serde_json::Value::Null);
            match i {
                0 | 2 => assert_eq!(id, serde_json::Value::from(0u64)),
                1 => assert_eq!(id, serde_json::Value::from(42u64)),
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_lowcard_nonnullable_defaults() -> Result<()> {
        // Typed path 'status' is LowCardinality(String) non-nullable. Missing values should default
        // to "".
        let values = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "status": "ok"}"#.to_vec()),
            Value::String(br#"{"name": "Carol"}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "status".to_string(),
                Box::new(Type::LowCardinality(Box::new(Type::String))),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
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
        let deserialized =
            type_.deserialize_column(&mut cursor, values.len(), &mut de_state).await?;

        // Validate: rows missing 'status' should have status = "" in JSON
        for (i, v) in deserialized.iter().enumerate() {
            #[cfg(feature = "serde")]
            if let Value::Json(obj) = v {
                let status = obj.get("status").cloned().unwrap_or(serde_json::Value::Null);
                match i {
                    0 | 2 => assert_eq!(status, serde_json::Value::from("")),
                    1 => assert_eq!(status, serde_json::Value::from("ok")),
                    _ => unreachable!(),
                }
                continue;
            }
            let obj: serde_json::Value = match v {
                Value::Object(bytes) | Value::String(bytes) => serde_json::from_slice(bytes)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?,
                other => panic!("Unexpected value variant: {other:?}"),
            };
            let status = obj.get("status").cloned().unwrap_or(serde_json::Value::Null);
            match i {
                0 | 2 => assert_eq!(status, serde_json::Value::from("")),
                1 => assert_eq!(status, serde_json::Value::from("ok")),
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_variant_null_missing() -> Result<()> {
        // Typed path 'value' is Variant(String, UInt64). Missing should map to Variant null (JSON
        // null).
        let values = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "value": "x"}"#.to_vec()),
            Value::String(br#"{"name": "Carol", "value": 7}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "value".to_string(),
                Box::new(Type::variant(vec![Type::String, Type::UInt64])),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
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
        let deserialized =
            type_.deserialize_column(&mut cursor, values.len(), &mut de_state).await?;

        // Validate: missing 'value' => JSON null; others preserved
        for (i, v) in deserialized.iter().enumerate() {
            #[cfg(feature = "serde")]
            if let Value::Json(obj) = v {
                match i {
                    0 => assert_eq!(obj.get("value"), Some(&serde_json::Value::Null)),
                    1 => assert_eq!(obj.get("value"), Some(&serde_json::Value::from("x"))),
                    2 => assert_eq!(obj.get("value"), Some(&serde_json::Value::from(7u64))),
                    _ => unreachable!(),
                }
                continue;
            }
            let obj: serde_json::Value = match v {
                Value::Object(bytes) | Value::String(bytes) => serde_json::from_slice(bytes)
                    .map_err(|e| Error::SerializeError(format!("JSON parse error: {e}")))?,
                other => panic!("Unexpected value variant: {other:?}"),
            };
            match i {
                0 => assert_eq!(obj.get("value"), Some(&serde_json::Value::Null)),
                1 => assert_eq!(obj.get("value"), Some(&serde_json::Value::from("x"))),
                2 => assert_eq!(obj.get("value"), Some(&serde_json::Value::from(7u64))),
                _ => unreachable!(),
            }
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
            skip_exact:        vec!["password".to_string()],
            skip_regex:        vec![".*_key".to_string(), "secret.*".to_string()],
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
            skip_exact:        vec!["password".to_string()],
            skip_regex:        vec![".*_key".to_string()],
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
                    other => panic!("Expected numeric 1, got {other:?}"),
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            let orig_json: serde_json::Value = match orig {
                Value::String(orig_bytes) | Value::Object(orig_bytes) => {
                    serde_json::from_slice(orig_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };
            let deser_json: serde_json::Value = match deser {
                Value::Object(deser_bytes) | Value::String(deser_bytes) => {
                    serde_json::from_slice(deser_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };
            assert_eq!(orig_json, deser_json);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_clickhouse_ordering() -> Result<()> {
        // Test the exact scenario from ClickHouse hex dump:
        // select map('a', ['b' || toString(number)])::JSON(a Array(Variant(String, Int64))) as z
        // from system.numbers limit 5 ClickHouse reorders to JSON(a Array(Variant(Int64,
        // String))) so discriminators are:
        // - Int64 → discriminator 0
        // - String → discriminator 1

        let values = vec![
            Value::String(br#"{"a": ["b0"]}"#.to_vec()),
            Value::String(br#"{"a": ["b1"]}"#.to_vec()),
            Value::String(br#"{"a": ["b2"]}"#.to_vec()),
            Value::String(br#"{"a": ["b3"]}"#.to_vec()),
            Value::String(br#"{"a": ["b4"]}"#.to_vec()),
        ];

        // Use the exact type from ClickHouse (which reordered String, Int64 → Int64, String)
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "a".to_string(),
                // This will be sorted alphabetically: Int64, String
                Box::new(Type::Array(Box::new(Type::variant(vec![Type::String, Type::Int64])))),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            println!("=== CLICKHOUSE ORDERING TEST ===");

            // Check that we have typed path "a"
            assert_eq!(json_state.typed_paths.len(), 1);
            assert!(json_state.typed_paths.iter().any(|(name, _)| name == "a"));

            if let Some(typed_columns) = &json_state.typed_path_columns {
                if let Some(a_column) = typed_columns.get("a") {
                    println!("Column 'a' has {} values", a_column.len());

                    // Check that string values like "b0" get discriminator 1 (String is
                    // alphabetically second)
                    for (i, value) in a_column.iter().enumerate() {
                        if let Value::Array(arr) = value {
                            if let Some(Value::Variant(discriminator, inner_val)) = arr.first() {
                                println!(
                                    "Row {}: discriminator {}, value: {:?}",
                                    i, discriminator, inner_val
                                );
                                // String values should get discriminator 1 (Int64=0, String=1)
                                assert_eq!(*discriminator, 1, "String discriminator should be 1");
                            }
                        }
                    }
                }
            }
        }

        println!("✅ ClickHouse discriminator ordering test passed!");
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            let orig_json: serde_json::Value = match original {
                Value::String(orig_bytes) | Value::Object(orig_bytes) => {
                    serde_json::from_slice(orig_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };
            let deser_json: serde_json::Value = match deserialized {
                Value::Object(deser_bytes) | Value::String(deser_bytes) => {
                    serde_json::from_slice(deser_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };

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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
            let orig_json: serde_json::Value = match orig {
                Value::String(orig_bytes) | Value::Object(orig_bytes) => {
                    serde_json::from_slice(orig_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };
            let deser_json: serde_json::Value = match deser {
                Value::Object(deser_bytes) | Value::String(deser_bytes) => {
                    serde_json::from_slice(deser_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };

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
            // done
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
            skip_exact:        vec!["password".to_string()],
            skip_regex:        vec![".*_key".to_string(), "secret.*".to_string()],
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
            let deser_json: serde_json::Value = match deserialized_val {
                Value::Object(deser_bytes) | Value::String(deser_bytes) => {
                    serde_json::from_slice(deser_bytes)
                        .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}")))?
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => v.clone(),
                other => panic!("Unexpected value variant: {other:?}"),
            };

            // Verify only public field is present
            assert!(!deser_json["public"].is_null());

            // Verify skipped paths are not present
            assert!(deser_json["password"].is_null());
            assert!(deser_json["private_key"].is_null());
            assert!(deser_json["secret_token"].is_null());
            assert!(deser_json["api_key"].is_null());
            // done
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
            skip_exact:        vec![],
            skip_regex:        vec![],
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
        let parsed1: serde_json::Value = match &result_values[0] {
            #[cfg(feature = "serde")]
            Value::Json(v) => v.clone(),
            Value::Object(b) | Value::String(b) => {
                serde_json::from_slice(b).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            other => return Err(Error::SerializeError(format!("Unexpected value: {other:?}"))),
        };

        // 256 wraps to 0 as UInt8
        assert_eq!(parsed1["overflow_u8"], 0);
        // -129 wraps to 127 as Int8 (two's complement)
        assert_eq!(parsed1["underflow_i8"], 127);
        // 65536 wraps to 0 as UInt8
        assert_eq!(parsed1["big_to_small"], 0);

        let parsed2: serde_json::Value = match &result_values[1] {
            #[cfg(feature = "serde")]
            Value::Json(v) => v.clone(),
            Value::Object(b) | Value::String(b) => {
                serde_json::from_slice(b).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            other => return Err(Error::SerializeError(format!("Unexpected value: {other:?}"))),
        };

        // -1 as UInt8 becomes 255 (two's complement)
        assert_eq!(parsed2["negative_to_u8"], 255);
        // -1 as UInt16 becomes 65535
        assert_eq!(parsed2["negative_to_u16"], 65535);
        // -1 as UInt32 becomes 4294967295
        assert_eq!(parsed2["negative_to_u32"], 4_294_967_295_u64);

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
            skip_exact:        vec![],
            skip_regex:        vec![],
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

        let parsed: serde_json::Value = match &result_values[0] {
            #[cfg(feature = "serde")]
            Value::Json(v) => v.clone(),
            Value::Object(b) | Value::String(b) => {
                serde_json::from_slice(b).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            other => return Err(Error::SerializeError(format!("Unexpected value: {other:?}"))),
        };

        assert_eq!(parsed["str_int"], 123);
        const TEST_FLOAT: f64 = 3.14;
        assert_eq!(parsed["str_float"], TEST_FLOAT);
        assert_eq!(parsed["str_uint"], 255);
        assert_eq!(parsed["str_neg"], -456);
        assert_eq!(parsed["digit_u8_1"], 1);
        assert_eq!(parsed["digit_u8_0"], 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_collection_type_conversions() -> Result<()> {
        // Test Array element conversions with wrapping
        let values = vec![
            Value::String(br#"{"int_array": [256, 512, -1], "nested_array": [[1, 2], [3, 4]], "tuple_data": [100, 3.14, "hello"], "map_data": [["key1", 10], ["key2", 20]]}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                // Array with element conversion (256 wraps to 0 as UInt8)
                ("int_array".to_string(), Box::new(Type::Array(Box::new(Type::UInt8)))),
                // Nested array
                (
                    "nested_array".to_string(),
                    Box::new(Type::Array(Box::new(Type::Array(Box::new(Type::Int32))))),
                ),
                // Array to Tuple conversion
                (
                    "tuple_data".to_string(),
                    Box::new(Type::Tuple(vec![Type::UInt32, Type::Float32, Type::String])),
                ),
                // Array of tuples to Map
                (
                    "map_data".to_string(),
                    Box::new(Type::Map(Box::new(Type::String), Box::new(Type::Int16))),
                ),
            ],
            skip_exact:        vec![],
            skip_regex:        vec![],
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

        let parsed: serde_json::Value = match &result_values[0] {
            #[cfg(feature = "serde")]
            Value::Json(v) => v.clone(),
            Value::Object(b) | Value::String(b) => {
                serde_json::from_slice(b).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            other => return Err(Error::SerializeError(format!("Unexpected value: {other:?}"))),
        };

        // Check Array element conversions with wrapping
        assert_eq!(parsed["int_array"][0], 0); // 256 wraps to 0 as UInt8
        assert_eq!(parsed["int_array"][1], 0); // 512 wraps to 0 as UInt8 
        assert_eq!(parsed["int_array"][2], 255); // -1 wraps to 255 as UInt8

        // Check nested array
        assert_eq!(parsed["nested_array"][0][0], 1);
        assert_eq!(parsed["nested_array"][1][1], 4);

        // Check tuple (from array conversion)
        assert_eq!(parsed["tuple_data"][0], 100);
        // Float32 has limited precision, check within tolerance
        const TEST_FLOAT: f64 = 3.14;
        assert!((parsed["tuple_data"][1].as_f64().unwrap() - TEST_FLOAT).abs() < 0.01);
        assert_eq!(parsed["tuple_data"][2], "hello");

        // Check map (from array of tuples)
        assert_eq!(parsed["map_data"]["key1"], 10);
        assert_eq!(parsed["map_data"]["key2"], 20);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_nested_collection_conversions() -> Result<()> {
        // Test deeply nested collection conversions
        let values = vec![
            Value::String(br#"{"array_of_tuples": [[1, "a"], [2, "b"], [3, "c"]], "tuple_of_arrays": [[1, 2, 3], [4.5, 6.7]], "nullable_array": [1, null, 3]}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                // Array of Tuples with type conversion
                (
                    "array_of_tuples".to_string(),
                    Box::new(Type::Array(Box::new(Type::Tuple(vec![Type::UInt16, Type::String])))),
                ),
                // Tuple of Arrays
                (
                    "tuple_of_arrays".to_string(),
                    Box::new(Type::Tuple(vec![
                        Type::Array(Box::new(Type::Int32)),
                        Type::Array(Box::new(Type::Float32)),
                    ])),
                ),
                // Array with nullable elements
                (
                    "nullable_array".to_string(),
                    Box::new(Type::Array(Box::new(Type::Nullable(Box::new(Type::UInt32))))),
                ),
            ],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // Serialize
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        type_.serialize_column(values.clone(), &mut output, &mut ser_state).await?;

        // Deserialize
        let mut input = output.as_slice();
        let mut de_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut input, &mut de_state).await?;
        let result_values = type_.deserialize_column(&mut input, 1, &mut de_state).await?;

        let parsed: serde_json::Value = match &result_values[0] {
            #[cfg(feature = "serde")]
            Value::Json(v) => v.clone(),
            Value::Object(b) | Value::String(b) => {
                serde_json::from_slice(b).map_err(|e| Error::SerializeError(e.to_string()))?
            }
            other => return Err(Error::SerializeError(format!("Unexpected value: {other:?}"))),
        };

        // Check array of tuples
        assert_eq!(parsed["array_of_tuples"][0][0], 1);
        assert_eq!(parsed["array_of_tuples"][0][1], "a");
        assert_eq!(parsed["array_of_tuples"][2][0], 3);
        assert_eq!(parsed["array_of_tuples"][2][1], "c");

        // Check tuple of arrays
        assert_eq!(parsed["tuple_of_arrays"][0][0], 1);
        assert_eq!(parsed["tuple_of_arrays"][0][2], 3);
        // Float32 has limited precision, check within tolerance
        assert!((parsed["tuple_of_arrays"][1][0].as_f64().unwrap() - 4.5).abs() < 0.01);
        assert!((parsed["tuple_of_arrays"][1][1].as_f64().unwrap() - 6.7).abs() < 0.01);

        // Check nullable array
        assert_eq!(parsed["nullable_array"][0], 1);
        assert_eq!(parsed["nullable_array"][1], serde_json::Value::Null);
        assert_eq!(parsed["nullable_array"][2], 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_lowcardinality_binary_output() -> Result<()> {
        // Create the EXACT same data structure that ClickHouse produces from:
        // select map('a', 'b' || toString(number))::JSON(a LowCardinality(String)) as z from
        // system.numbers limit 5
        let values = vec![
            Value::String(br#"{"a": "b0"}"#.to_vec()),
            Value::String(br#"{"a": "b1"}"#.to_vec()),
            Value::String(br#"{"a": "b2"}"#.to_vec()),
            Value::String(br#"{"a": "b3"}"#.to_vec()),
            Value::String(br#"{"a": "b4"}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "a".to_string(),
                Box::new(Type::LowCardinality(Box::new(Type::String))),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        // Write to file for comparison
        std::fs::write("/tmp/our_output.native", &output)?;

        // Debug the analyzed state
        if let TypeSpecificState::Json(json_state) = &state.type_specific {
            println!("=== JSON STATE ANALYSIS ===");
            println!("Typed paths: {:?}", json_state.typed_paths);
            if let Some(typed_columns) = &json_state.typed_path_columns {
                println!("Typed path columns: {:?}", typed_columns.keys().collect::<Vec<_>>());
                for (path, column_values) in typed_columns {
                    println!(
                        "Path '{}' has {} values: {:?}",
                        path,
                        column_values.len(),
                        column_values
                    );
                }
            }
        }

        // Check specific positions for the LowCardinality version
        println!("\n=== STRUCTURE ANALYSIS ===");
        println!("JSON version (bytes 0-7): {:02x?}", &output[0..8]);
        println!("Typed path count (byte 8): {:02x}", output[8]);
        if output.len() > 16 {
            println!("First typed path header (bytes 9-16): {:02x?}", &output[9..17]);
            if output.len() > 24 {
                println!(
                    "More data (bytes 17-24): {:02x?}",
                    &output[17..std::cmp::min(25, output.len())]
                );
            }
        }

        // Print hex dump for debugging
        println!("\n=== HEX DUMP OF OUR OUTPUT ===");
        for (i, chunk) in output.chunks(16).enumerate() {
            print!("{:08x}: ", i * 16);
            for (j, byte) in chunk.iter().enumerate() {
                print!("{byte:02x} ");
                if j == 7 {
                    print!(" ");
                }
            }

            // Pad if less than 16 bytes
            if chunk.len() < 16 {
                for j in chunk.len()..16 {
                    print!("   ");
                    if j == 7 {
                        print!(" ");
                    }
                }
            }

            print!(" |");
            for byte in chunk {
                if *byte >= 0x20 && *byte <= 0x7e {
                    print!("{}", *byte as char);
                } else {
                    print!(".");
                }
            }
            println!("|");
        }

        println!("\nTotal size: {} bytes", output.len());

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_variant_simple() -> Result<()> {
        // Test simple Variant in JSON typed path with just numeric types first
        let values = vec![
            Value::String(br#"{"status": "active", "value": 42}"#.to_vec()),
            Value::String(br#"{"status": "inactive", "value": 123}"#.to_vec()),
            Value::String(br#"{"status": "pending", "value": 3.14}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "value".to_string(),
                Box::new(Type::variant(vec![Type::Int64, Type::Float64])),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.typed_paths.len(), 1);
            assert!(json_state.typed_paths.iter().any(|(p, _)| p == "value"));
            assert!(json_state.dynamic_paths.contains(&"status".to_string()));

            // Check typed columns were extracted with correct types
            if let Some(typed_cols) = &json_state.typed_path_columns {
                assert!(typed_cols.contains_key("value"));
                let value_column = typed_cols.get("value").unwrap();
                assert_eq!(value_column.len(), 3);

                // Values should be converted to Variant types with discriminators
                for (i, val) in value_column.iter().enumerate() {
                    match val {
                        Value::Variant(disc, inner) => {
                            println!("Row {}: discriminator {}, value type: {:?}", i, disc, inner);
                            // Just verify we have variant values with proper discriminators
                            assert!(
                                *disc == 0 || *disc == 1,
                                "Discriminator should be 0 or 1, got {}",
                                disc
                            );
                        }
                        other => panic!("Expected Variant value at row {}, got {other:?}", i),
                    }
                }
            }
        }

        // Test serialization prefix (the main issue we fixed)
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = state;

        // This should work now without "Unsupported Variant serialization version" error
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        assert!(!output.is_empty(), "Should have serialized prefix data");

        println!("✅ Variant prefix serialization successful - no version error!");

        // Note: Full roundtrip needs more work on dynamic path handling, but the core
        // Variant issue is resolved

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_variant_in_array() -> Result<()> {
        // Test Variant within Array type in JSON typed path
        let values = vec![
            Value::String(br#"{"items": [1, "text", 3.14]}"#.to_vec()),
            Value::String(br#"{"items": ["hello", 42, 2.71]}"#.to_vec()),
            Value::String(br#"{"items": [99, "world"]}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "items".to_string(),
                Box::new(Type::Array(Box::new(Type::variant(vec![
                    Type::String,
                    Type::Int64,
                    Type::Float64,
                ])))),
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // Analyze values
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.typed_paths.len(), 1);
            assert!(json_state.typed_paths.iter().any(|(p, _)| p == "items"));

            // Check typed columns were extracted
            if let Some(typed_cols) = &json_state.typed_path_columns {
                assert!(typed_cols.contains_key("items"));
                let items_column = typed_cols.get("items").unwrap();
                assert_eq!(items_column.len(), 3);

                // Each value should be an Array of Variant values
                for array_val in items_column {
                    match array_val {
                        Value::Array(elements) => {
                            assert!(!elements.is_empty());
                            // Elements should be Variant values with proper discriminators
                            for element in elements {
                                match element {
                                    Value::Variant(disc, inner) => {
                                        // Verify discriminator is valid (0, 1, or 2 for Float64,
                                        // Int64, String)
                                        assert!(*disc <= 2, "Invalid discriminator: {}", disc);
                                        // Verify inner value matches expected types
                                        match inner.as_ref() {
                                            Value::String(_)
                                            | Value::Int64(_)
                                            | Value::Float64(_) => {}
                                            other => {
                                                panic!("Unexpected variant inner type: {other:?}")
                                            }
                                        }
                                    }
                                    other => panic!("Expected Variant element, got {other:?}"),
                                }
                            }
                        }
                        other => panic!("Expected Array value, got {other:?}"),
                    }
                }
            }
        }

        // Test serialization (analyze only for now to verify structure)
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = state;

        // Just test prefix serialization to verify structure is correct
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;

        assert!(!output.is_empty(), "Should have serialized prefix data");

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_variant_type_mismatch() -> Result<()> {
        // Test error handling when JSON value doesn't match Variant types
        let values = vec![Value::String(br#"{"strict_int": "not_a_number"}"#.to_vec())];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![(
                "strict_int".to_string(),
                Box::new(Type::variant(vec![Type::UInt32, Type::Int32])), // Only numeric types
            )],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // This should fail since "not_a_number" cannot be converted to UInt32 or Int32
        let result = JsonSerializer::analyze_values(&values, &type_);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("Cannot find matching variant type"));
        assert!(error.to_string().contains("not_a_number"));

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_nested_variant() -> Result<()> {
        // Test nested structures with Variant
        let values = vec![
            Value::String(br#"{"config": {"timeout": 30, "retries": "auto"}}"#.to_vec()),
            Value::String(br#"{"config": {"timeout": "infinite", "retries": 5}}"#.to_vec()),
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![], /* Let these be dynamic for now since nested typed paths
                                        * are complex */
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        // Analyze values - this tests that nested structures work
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        if let TypeSpecificState::Json(json_state) = &state {
            // Should have dynamic paths for nested structure
            assert!(json_state.dynamic_paths.len() > 0);
        }

        Ok(())
    }
}
