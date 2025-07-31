use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::formats::{JsonState as JsonStateData, TypeSpecificState};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::{Error, Result};

// JSON serialization versions
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
        value: &Value,
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
        let json_value = value.to_json()?;
        let old = current.insert(parts[parts.len() - 1].to_string(), json_value);
        debug_assert!(old.is_none() || matches!(old, Some(serde_json::Value::Null)));
        Ok(())
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
                    Self::set_nested_value(&mut row_object, path_name, value)?;
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

        if version != JSON_OBJECT_VERSION_3 {
            return Err(Error::DeserializeError(format!(
                "JSON type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            )));
        }

        // V3 format: just total_paths
        let total_paths = reader.read_var_uint().await?;

        // Read path names
        let mut path_names = Vec::with_capacity(total_paths.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_paths {
            let path_bytes = reader.read_string().await?;
            let path_name = String::from_utf8(path_bytes)
                .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 in path: {e}")))?;
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
            let mut types = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
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
            version:      Some(version),
            paths:        path_names,
            path_columns: None,
            rows:         None,
            dynamic_data: Some(dynamic_data),
        });
        Ok(())
    }

    async fn read<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, path_names, dynamic_data) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let version = json_state.version.ok_or_else(|| {
                    Error::DeserializeError(
                        "JSON version not set. read_prefix must be called first".to_string(),
                    )
                })?;
                (version, json_state.paths.clone(), json_state.dynamic_data.clone())
            } else {
                return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
            };

        match version {
            JSON_OBJECT_VERSION_3 => {
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
            _ => Err(Error::DeserializeError(format!(
                "JSON type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            ))),
        }
    }

    fn read_sync(
        _type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, path_names, dynamic_data) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let version = json_state.version.ok_or_else(|| {
                    Error::DeserializeError(
                        "JSON version not set. read_prefix must be called first".to_string(),
                    )
                })?;
                (version, json_state.paths.clone(), json_state.dynamic_data.clone())
            } else {
                return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
            };

        match version {
            JSON_OBJECT_VERSION_3 => {
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
            _ => Err(Error::DeserializeError(format!(
                "JSON type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            ))),
        }
    }
}
#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use chrono_tz::UTC;

    use super::*;
    use crate::native::values::{
        Date, Date32, DateTime, DynDateTime64, Ipv4, Ipv6, MultiPolygon, Point, Polygon, Ring,
        i256, u256,
    };

    #[test]
    fn test_value_to_json_numeric_types() {
        // Small integers
        assert_eq!(Value::Int8(42).to_json().unwrap(), serde_json::json!(42));
        assert_eq!(Value::Int16(-1000).to_json().unwrap(), serde_json::json!(-1000));
        assert_eq!(Value::Int32(123_456).to_json().unwrap(), serde_json::json!(123_456));
        assert_eq!(Value::Int64(-999_999_999).to_json().unwrap(), serde_json::json!(-999_999_999));

        assert_eq!(Value::UInt8(255).to_json().unwrap(), serde_json::json!(255));
        assert_eq!(Value::UInt16(65_535).to_json().unwrap(), serde_json::json!(65_535));
        assert_eq!(
            Value::UInt32(4_294_967_295).to_json().unwrap(),
            serde_json::json!(4_294_967_295_u32)
        );
        assert_eq!(
            Value::UInt64(18_446_744_073_709_551_615_u64).to_json().unwrap(),
            serde_json::json!(18_446_744_073_709_551_615_u64)
        );

        // Large integers as strings
        assert_eq!(
            Value::Int128(170_141_183_460_469_231_731_687_303_715_884_105_727_i128)
                .to_json()
                .unwrap(),
            serde_json::json!("170141183460469231731687303715884105727")
        );
        assert_eq!(
            Value::UInt128(340_282_366_920_938_463_463_374_607_431_768_211_455_u128)
                .to_json()
                .unwrap(),
            serde_json::json!("340282366920938463463374607431768211455")
        );

        // 256-bit integers
        let i256_val = i256([
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ]);
        let u256_val = u256([
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ]);
        assert!(Value::Int256(i256_val).to_json().unwrap().is_string());
        assert!(Value::UInt256(u256_val).to_json().unwrap().is_string());

        // Floats
        assert_eq!(
            Value::Float32(std::f32::consts::PI).to_json().unwrap(),
            serde_json::json!(std::f32::consts::PI)
        );
        assert_eq!(
            Value::Float64(-std::f64::consts::E).to_json().unwrap(),
            serde_json::json!(-std::f64::consts::E)
        );
    }

    #[test]
    fn test_value_to_json_decimal_types() {
        // Decimal32
        assert_eq!(Value::Decimal32(2, 1234).to_json().unwrap(), serde_json::json!("12.34"));
        assert_eq!(Value::Decimal32(0, 1234).to_json().unwrap(), serde_json::json!("1234"));
        assert_eq!(Value::Decimal32(4, 12).to_json().unwrap(), serde_json::json!("0.0012"));
        assert_eq!(Value::Decimal32(2, -1234).to_json().unwrap(), serde_json::json!("-12.34"));

        // Decimal64
        assert_eq!(
            Value::Decimal64(6, 123_456_789).to_json().unwrap(),
            serde_json::json!("123.456789")
        );
        assert_eq!(Value::Decimal64(10, 5).to_json().unwrap(), serde_json::json!("0.0000000005"));

        // Decimal128
        assert_eq!(Value::Decimal128(3, 123_456).to_json().unwrap(), serde_json::json!("123.456"));

        // Decimal256
        let d256 = i256([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 100,
        ]);
        assert_eq!(Value::Decimal256(2, d256).to_json().unwrap(), serde_json::json!("1.00"));
    }

    #[test]
    fn test_value_to_json_string_and_null() {
        assert_eq!(Value::Null.to_json().unwrap(), serde_json::json!(null));
        assert_eq!(
            Value::String(b"hello world".to_vec()).to_json().unwrap(),
            serde_json::json!("hello world")
        );
        assert_eq!(Value::String(b"".to_vec()).to_json().unwrap(), serde_json::json!(""));
    }

    #[test]
    fn test_value_to_json_date_time_types() {
        // Date
        let date = Date::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        assert_eq!(Value::Date(date).to_json().unwrap(), serde_json::json!("2024-01-15"));

        // Date32
        let date32 = Date32::from(chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        assert_eq!(Value::Date32(date32).to_json().unwrap(), serde_json::json!("2024-12-31"));

        // DateTime - ClickHouse format: "YYYY-MM-DD HH:MM:SS"
        let dt = DateTime(UTC, 1_705_320_600);
        let json_val = Value::DateTime(dt).to_json().unwrap();
        assert_eq!(json_val, serde_json::json!("2024-01-15 12:10:00"));

        // DateTime64 - ClickHouse format with precision
        let dt64 = DynDateTime64(UTC, 1_705_320_600_123, 3);
        let json_val = Value::DateTime64(dt64).to_json().unwrap();
        assert_eq!(json_val, serde_json::json!("2024-01-15 12:10:00.123"));
    }

    #[test]
    fn test_value_to_json_uuid_and_network() {
        // UUID
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            Value::Uuid(uuid).to_json().unwrap(),
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000")
        );

        // IPv4
        let ipv4 = Ipv4(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(Value::Ipv4(ipv4).to_json().unwrap(), serde_json::json!("192.168.1.1"));

        // IPv6
        let ipv6 = Ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        assert_eq!(Value::Ipv6(ipv6).to_json().unwrap(), serde_json::json!("2001:db8::1"));
    }

    #[test]
    fn test_value_to_json_enum_types() {
        assert_eq!(
            Value::Enum8("active".to_string(), 1).to_json().unwrap(),
            serde_json::json!("active")
        );
        assert_eq!(
            Value::Enum16("pending".to_string(), 2).to_json().unwrap(),
            serde_json::json!("pending")
        );
    }

    #[test]
    fn test_value_to_json_container_types() {
        // Array
        let array = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
        assert_eq!(Value::Array(array).to_json().unwrap(), serde_json::json!([1, 2, 3]));

        // Tuple
        let tuple = vec![
            Value::Int32(1),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ];
        assert_eq!(
            Value::Tuple(tuple).to_json().unwrap(),
            serde_json::json!([1, "hello", std::f64::consts::PI])
        );

        // Map with string keys - becomes object
        let keys = vec![Value::String(b"name".to_vec()), Value::String(b"age".to_vec())];
        let values = vec![Value::String(b"Alice".to_vec()), Value::Int32(30)];
        assert_eq!(
            Value::Map(keys, values).to_json().unwrap(),
            serde_json::json!({"name": "Alice", "age": 30})
        );

        // Map with non-string keys - ClickHouse converts keys to strings
        let keys = vec![Value::Int32(1), Value::Int32(2)];
        let values = vec![Value::String(b"one".to_vec()), Value::String(b"two".to_vec())];
        assert_eq!(
            Value::Map(keys, values).to_json().unwrap(),
            serde_json::json!({"1": "one", "2": "two"})
        );

        // Map with various key types
        let keys = vec![Value::UInt64(42), Value::Float64(3.5), Value::Null];
        let values = vec![
            Value::String(b"forty-two".to_vec()),
            Value::String(b"float".to_vec()),
            Value::String(b"null-key".to_vec()),
        ];
        assert_eq!(
            Value::Map(keys, values).to_json().unwrap(),
            serde_json::json!({"42": "forty-two", "3.5": "float", "null": "null-key"})
        );
    }

    #[test]
    fn test_value_to_json_variant() {
        let inner = Value::String(b"hello".to_vec());
        assert_eq!(
            Value::Variant(0, Box::new(inner)).to_json().unwrap(),
            serde_json::json!("hello")
        );

        let inner = Value::Int32(42);
        assert_eq!(Value::Variant(1, Box::new(inner)).to_json().unwrap(), serde_json::json!(42));
    }

    #[test]
    fn test_value_to_json_object() {
        let json_obj = b"{\"key\": \"value\", \"number\": 42}";
        let expected = serde_json::json!({"key": "value", "number": 42});
        assert_eq!(Value::Object(json_obj.to_vec()).to_json().unwrap(), expected);
    }

    #[test]
    fn test_value_to_json_geo_types() {
        // Point
        let point = Point([1.5, 2.5]);
        assert_eq!(Value::Point(point).to_json().unwrap(), serde_json::json!([1.5, 2.5]));

        // Ring
        let ring =
            Ring(vec![Point([0.0, 0.0]), Point([1.0, 0.0]), Point([1.0, 1.0]), Point([0.0, 0.0])]);
        assert_eq!(
            Value::Ring(ring).to_json().unwrap(),
            serde_json::json!([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]])
        );

        // Polygon
        let polygon = Polygon(vec![
            Ring(vec![
                Point([0.0, 0.0]),
                Point([4.0, 0.0]),
                Point([4.0, 4.0]),
                Point([0.0, 4.0]),
                Point([0.0, 0.0]),
            ]),
            Ring(vec![
                Point([1.0, 1.0]),
                Point([1.0, 2.0]),
                Point([2.0, 2.0]),
                Point([2.0, 1.0]),
                Point([1.0, 1.0]),
            ]),
        ]);
        let json_poly = Value::Polygon(polygon).to_json().unwrap();
        assert!(json_poly.is_array());
        assert_eq!(json_poly.as_array().unwrap().len(), 2); // outer ring + hole

        // MultiPolygon
        let multi = MultiPolygon(vec![Polygon(vec![Ring(vec![
            Point([0.0, 0.0]),
            Point([1.0, 0.0]),
            Point([1.0, 1.0]),
            Point([0.0, 0.0]),
        ])])]);
        let json_multi = Value::MultiPolygon(multi).to_json().unwrap();
        assert!(json_multi.is_array());
        assert_eq!(json_multi.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_value_to_json_nested_structures() {
        // Nested array
        let nested = Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]);
        assert_eq!(nested.to_json().unwrap(), serde_json::json!([[1, 2], [3, 4]]));

        // Array of tuples
        let array_of_tuples = Value::Array(vec![
            Value::Tuple(vec![Value::String(b"a".to_vec()), Value::Int32(1)]),
            Value::Tuple(vec![Value::String(b"b".to_vec()), Value::Int32(2)]),
        ]);
        assert_eq!(array_of_tuples.to_json().unwrap(), serde_json::json!([["a", 1], ["b", 2]]));
    }
}
