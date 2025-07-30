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

    /// Format a decimal value with proper decimal point placement
    fn format_decimal(mut value: String, scale: usize) -> String {
        if scale == 0 {
            return value;
        }

        let is_negative = value.starts_with('-');
        if is_negative {
            let _ = value.remove(0);
        }

        // Pad with leading zeros if needed
        while value.len() <= scale {
            value.insert(0, '0');
        }

        // Insert decimal point
        let point_pos = value.len() - scale;
        value.insert(point_pos, '.');

        // Add negative sign back if needed
        if is_negative {
            value.insert(0, '-');
        }

        value
    }

    /// Convert `ClickHouse` Value to JSON
    #[expect(clippy::too_many_lines)]
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

            // Decimal types - format with proper decimal point
            Value::Decimal32(scale, value) => {
                JsonValue::String(Self::format_decimal(value.to_string(), scale))
            }
            Value::Decimal64(scale, value) => {
                JsonValue::String(Self::format_decimal(value.to_string(), scale))
            }
            Value::Decimal128(scale, value) => {
                JsonValue::String(Self::format_decimal(value.to_string(), scale))
            }
            Value::Decimal256(scale, value) => {
                JsonValue::String(Self::format_decimal(value.to_string(), scale))
            }

            // Date/Time types - format as ISO strings
            Value::Date(date) => {
                let chrono_date: chrono::NaiveDate = date.into();
                JsonValue::String(chrono_date.format("%Y-%m-%d").to_string())
            }
            Value::Date32(date) => {
                let chrono_date: chrono::NaiveDate = date.into();
                JsonValue::String(chrono_date.format("%Y-%m-%d").to_string())
            }
            Value::DateTime(datetime) => {
                let chrono_date: chrono::DateTime<chrono_tz::Tz> = datetime
                    .try_into()
                    .map_err(|_| Error::DeserializeError("Invalid DateTime".to_string()))?;
                JsonValue::String(chrono_date.to_rfc3339())
            }
            Value::DateTime64(datetime) => {
                use crate::FromSql;
                let chrono_date: chrono::DateTime<chrono_tz::Tz> =
                    FromSql::from_sql(&Type::DateTime64(datetime.2, datetime.0), value.clone())
                        .map_err(|e| Error::DeserializeError(format!("Invalid DateTime64: {e}")))?;
                JsonValue::String(chrono_date.to_rfc3339())
            }

            // UUID - standard hyphenated format
            Value::Uuid(uuid) => JsonValue::String(uuid.to_string()),

            // Network types
            Value::Ipv4(ip) => JsonValue::String(ip.to_string()),
            Value::Ipv6(ip) => JsonValue::String(ip.to_string()),

            // Enum types - just the string value
            Value::Enum8(name, _) | Value::Enum16(name, _) => JsonValue::String(name),

            // Container types
            Value::Array(array) => {
                let mut arr = Vec::with_capacity(array.len());
                for item in array {
                    arr.push(Self::value_to_json(item)?);
                }
                JsonValue::Array(arr)
            }
            Value::Tuple(tuple) => {
                let mut arr = Vec::with_capacity(tuple.len());
                for item in tuple {
                    arr.push(Self::value_to_json(item)?);
                }
                JsonValue::Array(arr)
            }
            Value::Map(keys, values) => {
                // Check if all keys are strings
                let all_string_keys = keys.iter().all(|k| matches!(k, Value::String(_)));

                if all_string_keys && keys.len() == values.len() {
                    // Create JSON object
                    let mut map = serde_json::Map::new();
                    for (key, value) in keys.iter().zip(values.iter()) {
                        if let Value::String(key_bytes) = key {
                            let key_str = String::from_utf8(key_bytes.clone()).map_err(|e| {
                                Error::DeserializeError(format!("Invalid UTF-8 in map key: {e}"))
                            })?;
                            drop(map.insert(key_str, Self::value_to_json(value.clone())?));
                        }
                    }
                    JsonValue::Object(map)
                } else {
                    // Create array of [key, value] pairs
                    let mut arr = Vec::with_capacity(keys.len());
                    for (key, value) in keys.iter().zip(values.iter()) {
                        arr.push(JsonValue::Array(vec![
                            Self::value_to_json(key.clone())?,
                            Self::value_to_json(value.clone())?,
                        ]));
                    }
                    JsonValue::Array(arr)
                }
            }

            // Variant - unwrap and serialize contained value
            Value::Variant(_, boxed_value) => Self::value_to_json(*boxed_value)?,

            // Object type - already JSON, parse it
            Value::Object(json_bytes) => serde_json::from_slice(&json_bytes)
                .map_err(|e| Error::DeserializeError(format!("Invalid JSON in Object: {e}")))?,

            // Geo types - as coordinate arrays
            Value::Point(point) => JsonValue::Array(vec![
                JsonValue::Number(Number::from_f64(point.0[0]).unwrap_or(Number::from(0))),
                JsonValue::Number(Number::from_f64(point.0[1]).unwrap_or(Number::from(0))),
            ]),
            Value::Ring(ring) => {
                let mut arr = Vec::with_capacity(ring.0.len());
                for point in &ring.0 {
                    arr.push(JsonValue::Array(vec![
                        JsonValue::Number(Number::from_f64(point.0[0]).unwrap_or(Number::from(0))),
                        JsonValue::Number(Number::from_f64(point.0[1]).unwrap_or(Number::from(0))),
                    ]));
                }
                JsonValue::Array(arr)
            }
            Value::Polygon(polygon) => {
                let mut arr = Vec::with_capacity(polygon.0.len());
                for ring in &polygon.0 {
                    let mut ring_arr = Vec::with_capacity(ring.0.len());
                    for point in &ring.0 {
                        ring_arr.push(JsonValue::Array(vec![
                            JsonValue::Number(
                                Number::from_f64(point.0[0]).unwrap_or(Number::from(0)),
                            ),
                            JsonValue::Number(
                                Number::from_f64(point.0[1]).unwrap_or(Number::from(0)),
                            ),
                        ]));
                    }
                    arr.push(JsonValue::Array(ring_arr));
                }
                JsonValue::Array(arr)
            }
            Value::MultiPolygon(multi) => {
                let mut arr = Vec::with_capacity(multi.0.len());
                for polygon in &multi.0 {
                    let mut poly_arr = Vec::with_capacity(polygon.0.len());
                    for ring in &polygon.0 {
                        let mut ring_arr = Vec::with_capacity(ring.0.len());
                        for point in &ring.0 {
                            ring_arr.push(JsonValue::Array(vec![
                                JsonValue::Number(
                                    Number::from_f64(point.0[0]).unwrap_or(Number::from(0)),
                                ),
                                JsonValue::Number(
                                    Number::from_f64(point.0[1]).unwrap_or(Number::from(0)),
                                ),
                            ]));
                        }
                        poly_arr.push(JsonValue::Array(ring_arr));
                    }
                    arr.push(JsonValue::Array(poly_arr));
                }
                JsonValue::Array(arr)
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::values::{Date, Date32, DateTime, DynDateTime64, Ipv4, Ipv6, Point, Ring, Polygon, MultiPolygon, i256, u256};
    use chrono_tz::UTC;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_value_to_json_numeric_types() {
        // Small integers
        assert_eq!(JsonDeserializer::value_to_json(Value::Int8(42)).unwrap(), serde_json::json!(42));
        assert_eq!(JsonDeserializer::value_to_json(Value::Int16(-1000)).unwrap(), serde_json::json!(-1000));
        assert_eq!(JsonDeserializer::value_to_json(Value::Int32(123456)).unwrap(), serde_json::json!(123456));
        assert_eq!(JsonDeserializer::value_to_json(Value::Int64(-999999999)).unwrap(), serde_json::json!(-999999999));
        
        assert_eq!(JsonDeserializer::value_to_json(Value::UInt8(255)).unwrap(), serde_json::json!(255));
        assert_eq!(JsonDeserializer::value_to_json(Value::UInt16(65535)).unwrap(), serde_json::json!(65535));
        assert_eq!(JsonDeserializer::value_to_json(Value::UInt32(4294967295)).unwrap(), serde_json::json!(4294967295u32));
        assert_eq!(JsonDeserializer::value_to_json(Value::UInt64(18446744073709551615u64)).unwrap(), serde_json::json!(18446744073709551615u64));

        // Large integers as strings
        assert_eq!(JsonDeserializer::value_to_json(Value::Int128(170141183460469231731687303715884105727i128)).unwrap(), serde_json::json!("170141183460469231731687303715884105727"));
        assert_eq!(JsonDeserializer::value_to_json(Value::UInt128(340282366920938463463374607431768211455u128)).unwrap(), serde_json::json!("340282366920938463463374607431768211455"));
        
        // 256-bit integers
        let i256_val = i256([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32]);
        let u256_val = u256([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32]);
        assert!(JsonDeserializer::value_to_json(Value::Int256(i256_val)).unwrap().is_string());
        assert!(JsonDeserializer::value_to_json(Value::UInt256(u256_val)).unwrap().is_string());

        // Floats
        assert_eq!(JsonDeserializer::value_to_json(Value::Float32(3.14)).unwrap(), serde_json::json!(3.14f32));
        assert_eq!(JsonDeserializer::value_to_json(Value::Float64(-2.71828)).unwrap(), serde_json::json!(-2.71828));
    }

    #[test]
    fn test_value_to_json_decimal_types() {
        // Decimal32
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal32(2, 1234)).unwrap(), serde_json::json!("12.34"));
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal32(0, 1234)).unwrap(), serde_json::json!("1234"));
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal32(4, 12)).unwrap(), serde_json::json!("0.0012"));
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal32(2, -1234)).unwrap(), serde_json::json!("-12.34"));

        // Decimal64
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal64(6, 123456789)).unwrap(), serde_json::json!("123.456789"));
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal64(10, 5)).unwrap(), serde_json::json!("0.0000000005"));

        // Decimal128
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal128(3, 123456)).unwrap(), serde_json::json!("123.456"));
        
        // Decimal256
        let d256 = i256([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 100]);
        assert_eq!(JsonDeserializer::value_to_json(Value::Decimal256(2, d256)).unwrap(), serde_json::json!("1.00"));
    }

    #[test]
    fn test_value_to_json_string_and_null() {
        assert_eq!(JsonDeserializer::value_to_json(Value::Null).unwrap(), serde_json::json!(null));
        assert_eq!(JsonDeserializer::value_to_json(Value::String(b"hello world".to_vec())).unwrap(), serde_json::json!("hello world"));
        assert_eq!(JsonDeserializer::value_to_json(Value::String(b"".to_vec())).unwrap(), serde_json::json!(""));
    }

    #[test]
    fn test_value_to_json_date_time_types() {
        // Date
        let date = Date::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        assert_eq!(JsonDeserializer::value_to_json(Value::Date(date)).unwrap(), serde_json::json!("2024-01-15"));

        // Date32
        let date32 = Date32::from(chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        assert_eq!(JsonDeserializer::value_to_json(Value::Date32(date32)).unwrap(), serde_json::json!("2024-12-31"));

        // DateTime
        let dt = DateTime(UTC, 1705320600);
        let json_val = JsonDeserializer::value_to_json(Value::DateTime(dt)).unwrap();
        assert!(json_val.is_string());
        assert!(json_val.as_str().unwrap().contains("2024-01-15"));

        // DateTime64 - we'll test the format but not exact value due to timezone complexities
        let dt64 = DynDateTime64(UTC, 1705320600123, 3);
        let json_val = JsonDeserializer::value_to_json(Value::DateTime64(dt64)).unwrap();
        assert!(json_val.is_string());
        assert!(json_val.as_str().unwrap().contains("2024"));
    }

    #[test]
    fn test_value_to_json_uuid_and_network() {
        // UUID
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(JsonDeserializer::value_to_json(Value::Uuid(uuid)).unwrap(), serde_json::json!("550e8400-e29b-41d4-a716-446655440000"));

        // IPv4
        let ipv4 = Ipv4(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(JsonDeserializer::value_to_json(Value::Ipv4(ipv4)).unwrap(), serde_json::json!("192.168.1.1"));

        // IPv6
        let ipv6 = Ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        assert_eq!(JsonDeserializer::value_to_json(Value::Ipv6(ipv6)).unwrap(), serde_json::json!("2001:db8::1"));
    }

    #[test]
    fn test_value_to_json_enum_types() {
        assert_eq!(JsonDeserializer::value_to_json(Value::Enum8("active".to_string(), 1)).unwrap(), serde_json::json!("active"));
        assert_eq!(JsonDeserializer::value_to_json(Value::Enum16("pending".to_string(), 2)).unwrap(), serde_json::json!("pending"));
    }

    #[test]
    fn test_value_to_json_container_types() {
        // Array
        let array = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
        assert_eq!(JsonDeserializer::value_to_json(Value::Array(array)).unwrap(), serde_json::json!([1, 2, 3]));

        // Tuple
        let tuple = vec![Value::Int32(1), Value::String(b"hello".to_vec()), Value::Float64(3.14)];
        assert_eq!(JsonDeserializer::value_to_json(Value::Tuple(tuple)).unwrap(), serde_json::json!([1, "hello", 3.14]));

        // Map with string keys - becomes object
        let keys = vec![Value::String(b"name".to_vec()), Value::String(b"age".to_vec())];
        let values = vec![Value::String(b"Alice".to_vec()), Value::Int32(30)];
        assert_eq!(JsonDeserializer::value_to_json(Value::Map(keys, values)).unwrap(), serde_json::json!({"name": "Alice", "age": 30}));

        // Map with non-string keys - becomes array of pairs
        let keys = vec![Value::Int32(1), Value::Int32(2)];
        let values = vec![Value::String(b"one".to_vec()), Value::String(b"two".to_vec())];
        assert_eq!(JsonDeserializer::value_to_json(Value::Map(keys, values)).unwrap(), serde_json::json!([[1, "one"], [2, "two"]]));
    }

    #[test]
    fn test_value_to_json_variant() {
        let inner = Value::String(b"hello".to_vec());
        assert_eq!(JsonDeserializer::value_to_json(Value::Variant(0, Box::new(inner))).unwrap(), serde_json::json!("hello"));
        
        let inner = Value::Int32(42);
        assert_eq!(JsonDeserializer::value_to_json(Value::Variant(1, Box::new(inner))).unwrap(), serde_json::json!(42));
    }

    #[test]
    fn test_value_to_json_object() {
        let json_obj = b"{\"key\": \"value\", \"number\": 42}";
        let expected = serde_json::json!({"key": "value", "number": 42});
        assert_eq!(JsonDeserializer::value_to_json(Value::Object(json_obj.to_vec())).unwrap(), expected);
    }

    #[test]
    fn test_value_to_json_geo_types() {
        // Point
        let point = Point([1.5, 2.5]);
        assert_eq!(JsonDeserializer::value_to_json(Value::Point(point)).unwrap(), serde_json::json!([1.5, 2.5]));

        // Ring
        let ring = Ring(vec![Point([0.0, 0.0]), Point([1.0, 0.0]), Point([1.0, 1.0]), Point([0.0, 0.0])]);
        assert_eq!(JsonDeserializer::value_to_json(Value::Ring(ring)).unwrap(), serde_json::json!([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]));

        // Polygon
        let polygon = Polygon(vec![
            Ring(vec![Point([0.0, 0.0]), Point([4.0, 0.0]), Point([4.0, 4.0]), Point([0.0, 4.0]), Point([0.0, 0.0])]),
            Ring(vec![Point([1.0, 1.0]), Point([1.0, 2.0]), Point([2.0, 2.0]), Point([2.0, 1.0]), Point([1.0, 1.0])])
        ]);
        let json_poly = JsonDeserializer::value_to_json(Value::Polygon(polygon)).unwrap();
        assert!(json_poly.is_array());
        assert_eq!(json_poly.as_array().unwrap().len(), 2); // outer ring + hole

        // MultiPolygon
        let multi = MultiPolygon(vec![
            Polygon(vec![Ring(vec![Point([0.0, 0.0]), Point([1.0, 0.0]), Point([1.0, 1.0]), Point([0.0, 0.0])])])
        ]);
        let json_multi = JsonDeserializer::value_to_json(Value::MultiPolygon(multi)).unwrap();
        assert!(json_multi.is_array());
        assert_eq!(json_multi.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_value_to_json_nested_structures() {
        // Nested array
        let nested = Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)])
        ]);
        assert_eq!(JsonDeserializer::value_to_json(nested).unwrap(), serde_json::json!([[1, 2], [3, 4]]));

        // Array of tuples
        let array_of_tuples = Value::Array(vec![
            Value::Tuple(vec![Value::String(b"a".to_vec()), Value::Int32(1)]),
            Value::Tuple(vec![Value::String(b"b".to_vec()), Value::Int32(2)])
        ]);
        assert_eq!(JsonDeserializer::value_to_json(array_of_tuples).unwrap(), serde_json::json!([["a", 1], ["b", 2]]));
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
                    version:      Some(version),
                    paths:        Vec::new(),
                    path_columns: None,
                    rows:         None,
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
                    version:      Some(version),
                    paths:        path_names,
                    path_columns: None,
                    rows:         None,
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
