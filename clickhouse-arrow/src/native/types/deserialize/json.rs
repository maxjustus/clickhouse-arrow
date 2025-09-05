use std::collections::{BTreeMap, HashMap};

use tokio::io::AsyncReadExt;

use super::{
    ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type, read_discriminator,
};
use crate::formats::{JsonState as JsonStateData, TypeSpecificState};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::{Error, Result};

// JSON serialization versions
// Using FLATTENED format (version 3) for client compatibility
const JSON_OBJECT_VERSION_FLATTENED: u64 = 3;

pub(crate) struct JsonDeserializer;

impl JsonDeserializer {
    /// Common logic for reading JSON data (async version)
    async fn read_json_internal_async<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, typed_paths, path_names, dynamic_data) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let version = json_state.version.ok_or_else(|| {
                    Error::DeserializeError(
                        "JSON version not set. read_prefix must be called first".to_string(),
                    )
                })?;
                (
                    version,
                    json_state.typed_paths.clone(),
                    json_state.dynamic_paths.clone(),
                    json_state.dynamic_data.clone(),
                )
            } else {
                return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
            };

        match version {
            JSON_OBJECT_VERSION_FLATTENED => {
                let dynamic_data = dynamic_data.ok_or_else(|| {
                    Error::DeserializeError("JSON object data not set".to_string())
                })?;

                // Read values for each path
                let mut path_values = HashMap::new();

                // Typed path prefixes were already read in read_prefix; now read their data
                for (path_name, type_) in &typed_paths {
                    let mut typed_state = DeserializerState::default();
                    let values = type_.deserialize_column(reader, rows, &mut typed_state).await?;
                    let old = path_values.insert(path_name.clone(), values);
                    debug_assert!(old.is_none());
                }

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
                    for (idx, (_type_name, typ)) in types.iter().enumerate() {
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

                // Build JSON objects with all paths (typed and dynamic)
                let all_paths: Vec<String> = typed_paths
                    .iter()
                    .map(|(name, _)| name.clone())
                    .chain(path_names.iter().cloned())
                    .collect();
                Self::build_json_objects(&all_paths, &path_values, rows)
            }
            _ => Err(Error::DeserializeError(format!(
                "JSON type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            ))),
        }
    }

    /// Common logic for reading JSON data (sync version)
    fn read_json_internal_sync(
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, typed_paths, path_names, dynamic_data) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                let version = json_state.version.ok_or_else(|| {
                    Error::DeserializeError(
                        "JSON version not set. read_prefix must be called first".to_string(),
                    )
                })?;
                (
                    version,
                    json_state.typed_paths.clone(),
                    json_state.dynamic_paths.clone(),
                    json_state.dynamic_data.clone(),
                )
            } else {
                return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
            };

        match version {
            JSON_OBJECT_VERSION_FLATTENED => {
                let dynamic_data = dynamic_data.ok_or_else(|| {
                    Error::DeserializeError("JSON object data not set".to_string())
                })?;

                // Read values for each path
                let mut path_values = HashMap::new();

                // Typed path prefixes were already read in read_prefix; now read their data
                for (path_name, type_) in &typed_paths {
                    let mut typed_state = DeserializerState::default();
                    let values = type_.deserialize_column_sync(reader, rows, &mut typed_state)?;
                    let old = path_values.insert(path_name.clone(), values);
                    debug_assert!(old.is_none());
                }

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

                // Build JSON objects with all paths (typed and dynamic)
                let all_paths: Vec<String> = typed_paths
                    .iter()
                    .map(|(name, _)| name.clone())
                    .chain(path_names.iter().cloned())
                    .collect();
                Self::build_json_objects(&all_paths, &path_values, rows)
            }
            _ => Err(Error::DeserializeError(format!(
                "JSON type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            ))),
        }
    }

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
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.read_u64_le().await?;

        if version != JSON_OBJECT_VERSION_FLATTENED {
            return Err(Error::DeserializeError(format!(
                "JSON type requires FLATTENED format (version 3), got version {version}. Please \
                 use ClickHouse server >= 25.6"
            )));
        }

        // In v3 format, typed paths are NOT in ObjectStructure
        // They are implicit from the schema passed to the deserializer
        let typed_paths = match type_ {
            Type::JSON { typed_paths, .. } => typed_paths
                .iter()
                .map(|(name, boxed_type)| (name.clone(), *boxed_type.clone()))
                .collect(),
            _ => vec![],
        };

        // Read flattened (dynamic) paths from ObjectStructure (v3 FLATTENED)
        let total_paths = reader.read_var_uint().await?;

        // Read path names
        let mut all_path_names = Vec::with_capacity(total_paths.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_paths {
            let path_bytes = reader.read_string().await?;
            let path_name = String::from_utf8(path_bytes)
                .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 in path: {e}")))?;
            all_path_names.push(path_name);
        }

        // Separate typed and dynamic paths (typed are not listed in FLATTENED header)
        let typed_path_names: std::collections::HashSet<String> =
            typed_paths.iter().map(|(name, _)| name.clone()).collect();
        let dynamic_path_names: Vec<String> = all_path_names
            .iter()
            .filter(|name| !typed_path_names.contains(*name))
            .cloned()
            .collect();

        // Read typed path prefixes using their native serializers (always present)
        for (_path_name, type_) in &typed_paths {
            type_.deserialize_prefix_async(reader, state).await?;
        }

        // Read Dynamic headers for dynamic paths only
        let mut dynamic_data = Vec::with_capacity(dynamic_path_names.len());
        for path_name in &dynamic_path_names {
            // Read Dynamic version
            let dyn_version = reader.read_u64_le().await?;
            if dyn_version != 3 {
                return Err(Error::DeserializeError(format!(
                    "Expected Dynamic v3 for path '{path_name}', got {dyn_version}"
                )));
            }

            // Read types
            let total_types = reader.read_var_uint().await?;
            let mut type_list = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
            for _ in 0..total_types {
                type_list.push(Self::parse_type_entry(reader.read_string().await?)?);
            }

            // Read prefixes
            for (_, typ) in &type_list {
                typ.deserialize_prefix_async(reader, state).await?;
            }

            dynamic_data.push((total_types, type_list));
        }

        // Store metadata in state
        state.type_specific = TypeSpecificState::Json(JsonStateData {
            version: Some(version),
            dynamic_paths: dynamic_path_names.clone(),
            typed_paths,
            dynamic_path_columns: None,
            typed_path_columns: None,
            rows: None,
            dynamic_data: Some(dynamic_data),
            path_dynamic_states: BTreeMap::new(),
            typed_path_states: BTreeMap::new(), // Not used in deserialization
            // Deprecated fields - leave as default
            #[allow(deprecated)]
            paths: vec![],
            #[allow(deprecated)]
            path_columns: None,
        });
        Ok(())
    }

    async fn read<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        Self::read_json_internal_async(reader, rows, state).await
    }

    fn read_sync(
        _type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        Self::read_json_internal_sync(reader, rows, state)
    }
}

impl JsonDeserializer {
    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.get_u64_le();
        if version != JSON_OBJECT_VERSION_FLATTENED {
            return Err(Error::DeserializeError(format!(
                "JSON type requires FLATTENED format (version 3), got version {version}. Please \
                 use ClickHouse server >= 25.6"
            )));
        }

        // In v3 format, typed paths are NOT in ObjectStructure
        // They are implicit from the schema passed to the deserializer
        let typed_paths = match type_ {
            Type::JSON { typed_paths, .. } => typed_paths
                .iter()
                .map(|(name, boxed_type)| (name.clone(), *boxed_type.clone()))
                .collect(),
            _ => vec![],
        };

        // Read flattened (dynamic) paths from ObjectStructure (v3 FLATTENED)
        let total_paths = reader.try_get_var_uint()?;

        // Read path names
        let mut all_path_names = Vec::with_capacity(total_paths.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_paths {
            let path_bytes = reader.try_get_string()?;
            let path_name = String::from_utf8(path_bytes.to_vec())
                .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 in path: {e}")))?;
            all_path_names.push(path_name);
        }

        // Separate typed and dynamic paths (typed are not listed in FLATTENED header)
        let typed_path_names: std::collections::HashSet<String> =
            typed_paths.iter().map(|(name, _)| name.clone()).collect();
        let dynamic_path_names: Vec<String> = all_path_names
            .iter()
            .filter(|name| !typed_path_names.contains(*name))
            .cloned()
            .collect();

        // Read typed path prefixes using their native serializers (always present)
        for (_path_name, type_) in &typed_paths {
            type_.deserialize_prefix(reader)?;
        }

        // Read Dynamic headers for dynamic paths only
        let mut dynamic_data = Vec::with_capacity(dynamic_path_names.len());
        for path_name in &dynamic_path_names {
            // Read Dynamic version
            let dyn_version = reader.get_u64_le();
            if dyn_version != 3 {
                return Err(Error::DeserializeError(format!(
                    "Expected Dynamic v3 for path '{path_name}', got {dyn_version}"
                )));
            }

            // Read types
            let total_types = reader.try_get_var_uint()?;
            let mut type_list = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
            for _ in 0..total_types {
                type_list.push(Self::parse_type_entry(reader.try_get_string()?.to_vec())?);
            }

            // Read prefixes for nested types
            for (_, typ) in &type_list {
                typ.deserialize_prefix(reader)?;
            }

            dynamic_data.push((total_types, type_list));
        }

        // Store metadata in state
        state.type_specific = TypeSpecificState::Json(JsonStateData {
            version: Some(version),
            dynamic_paths: dynamic_path_names.clone(),
            typed_paths,
            dynamic_path_columns: None,
            typed_path_columns: None,
            rows: None,
            dynamic_data: Some(dynamic_data),
            path_dynamic_states: BTreeMap::new(),
            typed_path_states: BTreeMap::new(), // Not used in deserialization
            // Deprecated fields - leave as default
            #[allow(deprecated)]
            paths: vec![],
            #[allow(deprecated)]
            path_columns: None,
        });
        Ok(())
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

    macro_rules! json_conversion_test {
        ($name:ident, $($value:expr => $expected_json:expr),+ $(,)?) => {
            #[test]
            fn $name() {
                $(
                    assert_eq!($value.to_json().unwrap(), $expected_json, "Failed on: {}", stringify!($value));
                )+
            }
        };
    }

    json_conversion_test!(
        test_numeric_json_conversions,
        Value::Int8(42) => serde_json::json!(42),
        Value::Int16(-1000) => serde_json::json!(-1000),
        Value::Int32(123_456) => serde_json::json!(123_456),
        Value::Int64(-999_999_999) => serde_json::json!(-999_999_999),
        Value::UInt8(255) => serde_json::json!(255),
        Value::UInt16(65_535) => serde_json::json!(65_535),
        Value::UInt32(4_294_967_295) => serde_json::json!(4_294_967_295_u32),
        Value::UInt64(18_446_744_073_709_551_615_u64) => serde_json::json!(18_446_744_073_709_551_615_u64),
    );

    #[test]
    fn test_large_integer_json_conversions() {
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
    }

    json_conversion_test!(
        test_float_json_conversions,
        Value::Float32(std::f32::consts::PI) => serde_json::json!(std::f32::consts::PI),
        Value::Float64(-std::f64::consts::E) => serde_json::json!(-std::f64::consts::E),
    );

    json_conversion_test!(
        test_decimal_json_conversions,
        Value::Decimal32(2, 1234) => serde_json::json!("12.34"),
        Value::Decimal32(0, 1234) => serde_json::json!("1234"),
        Value::Decimal32(4, 12) => serde_json::json!("0.0012"),
        Value::Decimal32(2, -1234) => serde_json::json!("-12.34"),
        Value::Decimal64(6, 123_456_789) => serde_json::json!("123.456789"),
        Value::Decimal64(10, 5) => serde_json::json!("0.0000000005"),
        Value::Decimal128(3, 123_456) => serde_json::json!("123.456"),
    );

    #[test]
    fn test_decimal256_json_conversion() {
        let d256 = i256([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 100,
        ]);
        assert_eq!(Value::Decimal256(2, d256).to_json().unwrap(), serde_json::json!("1.00"));
    }

    json_conversion_test!(
        test_string_null_json_conversions,
        Value::Null => serde_json::json!(null),
        Value::String(b"hello world".to_vec()) => serde_json::json!("hello world"),
        Value::String(b"".to_vec()) => serde_json::json!(""),
    );

    json_conversion_test!(
        test_datetime_json_conversions,
        Value::Date(Date::from(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap())) => serde_json::json!("2024-01-15"),
        Value::Date32(Date32::from(chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap())) => serde_json::json!("2024-12-31"),
        Value::DateTime(DateTime(UTC, 1_705_320_600)) => serde_json::json!("2024-01-15 12:10:00"),
        Value::DateTime64(DynDateTime64(UTC, 1_705_320_600_123, 3)) => serde_json::json!("2024-01-15 12:10:00.123"),
    );

    json_conversion_test!(
        test_uuid_network_json_conversions,
        Value::Uuid(uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()) => serde_json::json!("550e8400-e29b-41d4-a716-446655440000"),
        Value::Ipv4(Ipv4(Ipv4Addr::new(192, 168, 1, 1))) => serde_json::json!("192.168.1.1"),
        Value::Ipv6(Ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))) => serde_json::json!("2001:db8::1"),
    );

    json_conversion_test!(
        test_enum_json_conversions,
        Value::Enum8("active".to_string(), 1) => serde_json::json!("active"),
        Value::Enum16("pending".to_string(), 2) => serde_json::json!("pending"),
    );

    json_conversion_test!(
        test_container_json_conversions,
        Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]) => serde_json::json!([1, 2, 3]),
        Value::Tuple(vec![
            Value::Int32(1),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ]) => serde_json::json!([1, "hello", std::f64::consts::PI]),
        Value::Map(
            vec![Value::String(b"name".to_vec()), Value::String(b"age".to_vec())],
            vec![Value::String(b"Alice".to_vec()), Value::Int32(30)],
        ) => serde_json::json!({"name": "Alice", "age": 30}),
        Value::Map(
            vec![Value::Int32(1), Value::Int32(2)],
            vec![Value::String(b"one".to_vec()), Value::String(b"two".to_vec())],
        ) => serde_json::json!({"1": "one", "2": "two"}),
        Value::Map(
            vec![Value::UInt64(42), Value::Float64(3.5), Value::Null],
            vec![
                Value::String(b"forty-two".to_vec()),
                Value::String(b"float".to_vec()),
                Value::String(b"null-key".to_vec()),
            ],
        ) => serde_json::json!({"42": "forty-two", "3.5": "float", "null": "null-key"}),
    );

    json_conversion_test!(
        test_variant_object_json_conversions,
        Value::Variant(0, Box::new(Value::String(b"hello".to_vec()))) => serde_json::json!("hello"),
        Value::Variant(1, Box::new(Value::Int32(42))) => serde_json::json!(42),
    );

    #[test]
    fn test_object_json_conversion() {
        let json_obj = b"{\"key\": \"value\", \"number\": 42}";
        let expected = serde_json::json!({"key": "value", "number": 42});
        assert_eq!(Value::Object(json_obj.to_vec()).to_json().unwrap(), expected);
    }

    json_conversion_test!(
        test_geo_json_conversions,
        Value::Point(Point([1.5, 2.5])) => serde_json::json!([1.5, 2.5]),
        Value::Ring(Ring(vec![Point([0.0, 0.0]), Point([1.0, 0.0]), Point([1.0, 1.0]), Point([0.0, 0.0])])) => serde_json::json!([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]),
    );

    #[test]
    fn test_polygon_json_conversion() {
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
    }

    #[test]
    fn test_multipolygon_json_conversion() {
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

    json_conversion_test!(
        test_nested_json_conversions,
        Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]) => serde_json::json!([[1, 2], [3, 4]]),
        Value::Array(vec![
            Value::Tuple(vec![Value::String(b"a".to_vec()), Value::Int32(1)]),
            Value::Tuple(vec![Value::String(b"b".to_vec()), Value::Int32(2)]),
        ]) => serde_json::json!([["a", 1], ["b", 2]]),
    );

    #[test]
    fn test_json_read_prefix_sync() {
        use std::io::Cursor;

        use super::*;
        use crate::formats::{DeserializerState, TypeSpecificState};

        // Test data matching the async version
        let mut buffer = Vec::new();

        // Write JSON_OBJECT_VERSION_FLATTENED
        buffer.extend_from_slice(&JSON_OBJECT_VERSION_FLATTENED.to_le_bytes());

        // Write total_paths (1 path)
        buffer.extend_from_slice(&[1u8]); // varint 1

        // Write path name "user.name"
        let path_name = b"user.name";
        #[allow(clippy::cast_possible_truncation)]
        {
            buffer.extend_from_slice(&[path_name.len() as u8]); // varint length
        }
        buffer.extend_from_slice(path_name);

        // Write Dynamic version (3)
        buffer.extend_from_slice(&3u64.to_le_bytes());

        // Write total_types (1 type)
        buffer.extend_from_slice(&[1u8]); // varint 1

        // Write type name "String"
        let type_name = b"String";
        #[allow(clippy::cast_possible_truncation)]
        {
            buffer.extend_from_slice(&[type_name.len() as u8]); // varint length
        }
        buffer.extend_from_slice(type_name);

        let mut reader = Cursor::new(buffer);
        let mut state = DeserializerState::default();
        let json_type = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       Vec::default(),
            skip_paths:        Vec::default(),
        };

        // Test the sync prefix reading
        let result = JsonDeserializer::read_prefix_sync(&json_type, &mut reader, &mut state);
        assert!(result.is_ok(), "read_prefix_sync should succeed");

        // Validate state was properly set
        match &state.type_specific {
            TypeSpecificState::Json(json_state) => {
                assert_eq!(json_state.version, Some(JSON_OBJECT_VERSION_FLATTENED));
                assert_eq!(json_state.dynamic_paths, vec!["user.name"]);
                assert!(json_state.dynamic_data.is_some());
                let dynamic_data = json_state.dynamic_data.as_ref().unwrap();
                assert_eq!(dynamic_data.len(), 1);
                assert_eq!(dynamic_data[0].0, 1); // total_types
                assert_eq!(dynamic_data[0].1.len(), 1); // types vec
                assert_eq!(dynamic_data[0].1[0].0, "String"); // type name
            }
            _ => panic!("Expected Json state"),
        }
    }

    #[test]
    fn test_json_read_prefix_sync_invalid_version() {
        use std::io::Cursor;

        use super::*;
        use crate::formats::DeserializerState;

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&2u64.to_le_bytes()); // Invalid version

        let mut reader = Cursor::new(buffer);
        let mut state = DeserializerState::default();
        let json_type = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       Vec::default(),
            skip_paths:        Vec::default(),
        };

        let result = JsonDeserializer::read_prefix_sync(&json_type, &mut reader, &mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version 3"));
    }

    // Note: JSON sync roundtrip testing is handled by the integration test
    // in src/native/types/tests.rs (roundtrip_complex_types_sync)
}
