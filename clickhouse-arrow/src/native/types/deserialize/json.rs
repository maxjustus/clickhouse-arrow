use std::collections::BTreeMap;

use tokio::io::AsyncReadExt;

use super::{
    ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type, read_discriminator,
};
use crate::formats::{JsonState as JsonStateData, TypeSpecificState};
use crate::io::ClickHouseRead;
use crate::native::values::Value;
use crate::{Error, Result};

// JSON serialization versions
const JSON_OBJECT_VERSION_V1: u64 = 0; // Legacy with extra max_dynamic_paths field
const JSON_OBJECT_VERSION_STRING: u64 = 1;
const JSON_OBJECT_VERSION_V2: u64 = 2; // Modern with shared data
const JSON_OBJECT_VERSION_FLATTENED: u64 = 3;

pub(crate) struct JsonDeserializer;

enum SegmentRef<'a> {
    Borrowed(&'a [String]),
    Fallback(usize),
}

struct PathEntry<'a> {
    segments: SegmentRef<'a>,
    values:   &'a [Value],
}

impl JsonDeserializer {
    /// Common logic for reading JSON data (async version)
    async fn read_json_internal_async<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let json_state = if let TypeSpecificState::Json(json_state) = &state.type_specific {
            json_state.clone()
        } else {
            return Err(Error::DeserializeError("JSON metadata not set in state".to_string()));
        };

        let version = json_state.version.ok_or_else(|| {
            Error::DeserializeError(
                "JSON version not set. read_prefix must be called first".to_string(),
            )
        })?;

        match version {
            JSON_OBJECT_VERSION_V1 | JSON_OBJECT_VERSION_V2 => {
                Self::read_shared_data_format(reader, rows, &json_state).await
            }
            JSON_OBJECT_VERSION_FLATTENED => {
                let dynamic_data = json_state.dynamic_data.clone().ok_or_else(|| {
                    Error::DeserializeError("JSON object data not set".to_string())
                })?;
                let typed_paths = json_state.typed_paths.clone();
                let path_names = json_state.dynamic_paths.clone();
                let path_segments = json_state.path_segments.clone();

                let mut path_values = Vec::with_capacity(typed_paths.len() + path_names.len());

                // Typed path prefixes were already read in read_prefix; now read their data
                for (_path_name, type_) in &typed_paths {
                    let mut typed_state = DeserializerState::default();
                    let values = type_.deserialize_column(reader, rows, &mut typed_state).await?;
                    path_values.push(values);
                }

                for (path_idx, _path_name) in path_names.iter().enumerate() {
                    let (total_types, types) = &dynamic_data[path_idx];

                    // Read discriminators
                    let mut discriminators = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        discriminators.push(read_discriminator!(async reader, *total_types));
                    }

                    // Prepare offset bookkeeping
                    let total_types_usize = (*total_types).try_into().map_err(|_| {
                        Error::DeserializeError("Too many dynamic types in JSON column".to_string())
                    })?;
                    let (offsets, row_count_by_type) =
                        Self::build_offsets(&discriminators, *total_types, total_types_usize);

                    // Read column data
                    let mut columns = vec![Vec::new(); total_types_usize];
                    for (idx, (_type_name, typ)) in types.iter().enumerate() {
                        if let Some(&count) = row_count_by_type.get(idx)
                            && count > 0
                        {
                            let values = typ.deserialize_column(reader, count, state).await?;
                            columns[idx] = values;
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
                    path_values.push(values);
                }

                // Build JSON objects with all paths (typed and dynamic)
                let all_paths: Vec<String> = typed_paths
                    .iter()
                    .map(|(name, _)| name.clone())
                    .chain(path_names.iter().cloned())
                    .collect();
                Self::build_json_objects(&all_paths, &path_values, rows, &path_segments)
            }
            JSON_OBJECT_VERSION_STRING => {
                let mut result = Vec::with_capacity(rows);
                for _ in 0..rows {
                    let raw = reader.read_string().await?;

                    let parsed: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
                        Error::DeserializeError(format!(
                            "Failed to parse JSON string serialization: {e}"
                        ))
                    })?;

                    result.push(Value::Json(parsed));
                }

                Ok(result)
            }
            _ => Err(Error::DeserializeError(format!(
                "Unsupported JSON serialization version {version}."
            ))),
        }
    }

    /// Optimized nested setter using pre-split segments and Map::entry
    fn set_nested_value_segments(
        object: &mut serde_json::Map<String, serde_json::Value>,
        segments: &[String],
        value: &Value,
    ) -> Result<()> {
        use serde_json::Value as J;
        if segments.is_empty() {
            return Err(Error::DeserializeError("Empty path".to_string()));
        }

        let mut current = object;
        for part in &segments[..segments.len() - 1] {
            use serde_json::map::Entry;
            // entry() returns a temporary borrows; get a &mut Value and match it immediately
            let value_ref = match current.entry(part.clone()) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(J::Object(serde_json::Map::new())),
            };
            current = match value_ref {
                J::Object(map) => map,
                _ => {
                    return Err(Error::DeserializeError(format!(
                        "Path conflict: '{part}' is not an object"
                    )));
                }
            };
        }

        // TODO: make sure I understand this
        let leaf = segments.last().unwrap();
        let old = current.insert(leaf.clone(), value.to_json()?);
        debug_assert!(old.is_none() || matches!(old, Some(J::Null)));
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
        total_types_len: usize,
    ) -> (Vec<usize>, Vec<usize>) {
        let mut row_count_by_type = vec![0usize; total_types_len];
        let mut offsets = vec![0; discriminators.len()];

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == total_types {
                continue;
            }

            let disc_idx = disc as usize;
            if let Some(count) = row_count_by_type.get_mut(disc_idx) {
                offsets[i] = *count;
                *count += 1;
            } else {
                offsets[i] = 0;
            }
        }

        (offsets, row_count_by_type)
    }

    /// Reconstruct values from columns
    fn reconstruct_path_values(
        discriminators: &[u64],
        offsets: &[usize],
        columns: &[Vec<Value>],
        total_types: u64,
        rows: usize,
    ) -> Vec<Value> {
        let mut values = Vec::with_capacity(rows);

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == total_types {
                values.push(Value::Null);
            } else if let Some(column) = columns.get(disc as usize) {
                let offset = offsets[i];
                values.push(column.get(offset).cloned().unwrap_or(Value::Null));
            } else {
                values.push(Value::Null);
            }
        }

        values
    }

    /// Build JSON object from path values
    /// Emits `Value::Json(..)` when serde is enabled, otherwise `Value::Object(Vec<u8>)`.
    fn build_json_objects(
        path_names: &[String],
        path_values: &[Vec<Value>],
        rows: usize,
        path_segments: &BTreeMap<String, Vec<String>>,
    ) -> Result<Vec<Value>> {
        let mut result = Vec::with_capacity(rows);

        let mut entries = Vec::with_capacity(path_names.len());
        let mut fallback_segments: Vec<Vec<String>> = Vec::new();
        for (idx, path_name) in path_names.iter().enumerate() {
            let segment_ref = if let Some(segments) = path_segments.get(path_name) {
                SegmentRef::Borrowed(segments.as_slice())
            } else {
                fallback_segments.push(path_name.split('.').map(|s| s.to_string()).collect());
                SegmentRef::Fallback(fallback_segments.len() - 1)
            };
            let values = path_values.get(idx).ok_or_else(|| {
                Error::DeserializeError(format!("Path values missing for '{path_name}'"))
            })?;
            entries.push(PathEntry { segments: segment_ref, values: values.as_slice() });
        }

        for row_idx in 0..rows {
            let mut row_object = serde_json::Map::new();

            for entry in &entries {
                if let Some(value) = entry.values.get(row_idx)
                    && !matches!(value, Value::Null)
                {
                    let segments = match entry.segments {
                        SegmentRef::Borrowed(seg) => seg,
                        SegmentRef::Fallback(idx) => &fallback_segments[idx],
                    };
                    Self::set_nested_value_segments(&mut row_object, segments, value)?;
                }
            }

            result.push(Value::Json(serde_json::Value::Object(row_object)));
        }

        Ok(result)
    }
}

impl JsonDeserializer {
    /// Read V1/V2 format with shared data Map
    async fn read_shared_data_format<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
        json_state: &JsonStateData,
    ) -> Result<Vec<Value>> {
        use crate::native::types::deserialize::binary_value::deserialize_binary_value;

        let typed_paths = &json_state.typed_paths;
        let dynamic_path_names = &json_state.dynamic_paths;
        let path_segments = &json_state.path_segments;

        // 1. Read typed path columns
        let mut typed_values = Vec::with_capacity(typed_paths.len());
        for (_path_name, type_) in typed_paths {
            let mut typed_state = DeserializerState::default();
            let values = type_.deserialize_column(reader, rows, &mut typed_state).await?;
            typed_values.push(values);
        }

        // 2. V1/V2: Dynamic paths are NOT serialized as separate columns
        // Despite what SerializationObject.cpp suggests, testing shows that for V1/V2,
        // all dynamic paths (those listed in num_dynamic_paths) are stored in the shared data Map
        // The num_dynamic_paths field is for statistics/metadata only
        // Dynamic version 2 is used internally but columns are not serialized separately
        let dynamic_values: Vec<Vec<Value>> = Vec::new();

        // 3. Read shared data Map(String, String) - ALWAYS written by ClickHouse
        // Per SerializationObject.cpp:
        // shared_data_serialization->serializeBinaryBulkWithMultipleStreams()
        // is called unconditionally, even when num_dynamic_paths==0
        // Note: Map prefix was already read in read_prefix_shared_data()
        let shared_data = Self::read_shared_data_map(reader, rows).await?;

        // 4. Reconstruct JSON objects by merging typed paths, dynamic paths, and shared data
        let mut result = Vec::with_capacity(rows);
        for row_idx in 0..rows {
            let mut json_obj = serde_json::Map::new();

            // Add typed path values
            for (idx, (path_name, _type_)) in typed_paths.iter().enumerate() {
                let value = &typed_values[idx][row_idx];
                if let Some(segments) = path_segments.get(path_name) {
                    Self::set_nested_value_segments(&mut json_obj, segments, value)?;
                }
            }

            // Add dynamic path values (V1/V2: none, all in shared data; V3: separate columns)
            for (idx, path_name) in dynamic_path_names.iter().enumerate() {
                // Skip if no dynamic_values (V1/V2 case where everything is in shared data)
                if idx >= dynamic_values.len() {
                    break;
                }
                let value = &dynamic_values[idx][row_idx];
                if !matches!(value, Value::Null) {
                    if let Some(segments) = path_segments.get(path_name) {
                        Self::set_nested_value_segments(&mut json_obj, segments, value)?;
                    } else {
                        // Path not in segments map, split it
                        let segments: Vec<String> =
                            path_name.split('.').map(|s| s.to_string()).collect();
                        Self::set_nested_value_segments(&mut json_obj, &segments, value)?;
                    }
                }
            }

            // Add shared data values (remaining paths not in typed or dynamic)
            if let Some(row_shared_data) = shared_data.get(row_idx) {
                for (path_name, binary_value_bytes) in row_shared_data {
                    // Deserialize the binary value
                    let value = deserialize_binary_value(binary_value_bytes).map_err(|e| {
                        Error::DeserializeError(format!(
                            "Failed to deserialize shared data value for path {}: {}",
                            path_name, e
                        ))
                    })?;

                    // Set it in the JSON object
                    if let Some(segments) = path_segments.get(path_name) {
                        Self::set_nested_value_segments(&mut json_obj, segments, &value)?;
                    } else {
                        // Path not in segments map, split it
                        let segments: Vec<String> =
                            path_name.split('.').map(|s| s.to_string()).collect();
                        Self::set_nested_value_segments(&mut json_obj, &segments, &value)?;
                    }
                }
            }

            result.push(Value::Json(serde_json::Value::Object(json_obj)));
        }

        Ok(result)
    }

    /// Read the shared data Map(String, String) for V1/V2 formats
    /// Returns a vector of maps (one per row), where each map contains path -> binary_value_bytes
    async fn read_shared_data_map<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
    ) -> Result<Vec<std::collections::HashMap<String, Vec<u8>>>> {
        use std::collections::HashMap;

        // Shared data is Map(String, String) serialized as Array(Tuple(String, String))
        // Stream 1: ArraySizes (cumulative offsets, u64 each)
        let mut offsets = Vec::with_capacity(rows);
        for _ in 0..rows {
            offsets.push(reader.read_u64_le().await?);
        }

        let total_entries = offsets.last().copied().unwrap_or(0) as usize;

        // Stream 2: Keys (String column - path names)
        let mut keys = Vec::with_capacity(total_entries);
        for _ in 0..total_entries {
            let key_bytes = reader.read_string().await?;
            let key = String::from_utf8(key_bytes).map_err(|e| {
                Error::DeserializeError(format!("Invalid UTF-8 in shared data key: {e}"))
            })?;
            keys.push(key);
        }

        // Stream 3: Values (String column - binary encoded values)
        let mut values = Vec::with_capacity(total_entries);
        for _ in 0..total_entries {
            let value_bytes = reader.read_string().await?;
            values.push(value_bytes);
        }

        // Group by row using offsets
        let mut result = Vec::with_capacity(rows);
        let mut prev_offset = 0;
        for offset in offsets {
            let offset = offset as usize;
            let mut row_map = HashMap::new();
            for i in prev_offset..offset {
                drop(row_map.insert(keys[i].clone(), values[i].clone()));
            }
            result.push(row_map);
            prev_offset = offset;
        }

        Ok(result)
    }

    /// Read prefix for V1 and V2 (shared data formats)
    async fn read_prefix_shared_data<R: ClickHouseRead>(
        version: u64,
        typed_paths: Vec<(String, Type)>,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // V1 has an extra max_dynamic_paths field that we skip
        if version == JSON_OBJECT_VERSION_V1 {
            let _max_dynamic_paths = reader.read_var_uint().await?;
        }

        // Read number of dynamic paths
        let num_dynamic_paths = reader.read_var_uint().await?;

        // Read dynamic path names
        let mut dynamic_path_names =
            Vec::with_capacity(num_dynamic_paths.try_into().unwrap_or(usize::MAX));
        for _ in 0..num_dynamic_paths {
            let path_bytes = reader.read_string().await?;
            let path_name = String::from_utf8(path_bytes)
                .map_err(|e| Error::DeserializeError(format!("Invalid UTF-8 in path: {e}")))?;
            dynamic_path_names.push(path_name);
        }

        // NOTE: Statistics are conditionally written based on object_and_dynamic_write_statistics setting
        // Per SerializationObject.cpp lines 721-745, they're only read if object_and_dynamic_read_statistics is true
        // For Native format, ClickHouse appears to NOT write statistics by default
        // If we encounter issues, we may need to detect/handle statistics presence

        // Read typed path prefixes using their native serializers (alphabetically sorted)
        for (_path_name, type_) in &typed_paths {
            type_.deserialize_prefix_async(reader, state).await?;
        }

        // V1/V2 with dynamic paths is not yet supported
        // Dynamic columns in V1/V2 use Dynamic version 2, but our deserializer only supports version 3
        // For now, error out if there are any dynamic paths
        if num_dynamic_paths > 0 {
            return Err(Error::DeserializeError(format!(
                "V1/V2 JSON Object format with dynamic paths (num_dynamic_paths={}) is not yet \
                 supported. Please use V3 (FLATTENED) format by setting client_protocol_version >= \
                 54473 and output_format_native_use_flattened_dynamic_and_json_serialization = 1",
                num_dynamic_paths
            )));
        }
        let path_dynamic_states = BTreeMap::new();

        // Read the shared data Map(String, String) prefix
        // Per SerializationObject.cpp lines 649-652, shared_data_serialization->deserializeBinaryBulkStatePrefix
        // is called in the PREFIX phase
        let map_type = Type::Map(Box::new(Type::String), Box::new(Type::String));
        map_type.deserialize_prefix_async(reader, state).await?;

        // Build and cache path segments for typed and dynamic paths
        let mut path_segments = BTreeMap::new();
        for (name, _) in &typed_paths {
            drop(
                path_segments
                    .insert(name.clone(), name.split('.').map(|s| s.to_string()).collect()),
            );
        }
        for name in &dynamic_path_names {
            drop(
                path_segments
                    .insert(name.clone(), name.split('.').map(|s| s.to_string()).collect()),
            );
        }

        // Store metadata in state
        state.type_specific = TypeSpecificState::Json(JsonStateData {
            version: Some(version),
            dynamic_paths: dynamic_path_names,
            typed_paths,
            dynamic_path_columns: None,
            typed_path_columns: None,
            rows: None,
            dynamic_data: None, // V1/V2 don't use this field
            path_dynamic_states,  // Use the states we just created
            typed_path_states: BTreeMap::new(),
            path_segments,
            #[allow(deprecated)]
            paths: vec![],
            #[allow(deprecated)]
            path_columns: None,
        });
        Ok(())
    }
}

impl Deserializer for JsonDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.read_u64_le().await?;

        // Schema-driven typed paths are available even for legacy/string serialization
        let typed_paths: Vec<(String, Type)> = match type_ {
            Type::JSON { typed_paths, .. } => typed_paths
                .iter()
                .map(|(name, boxed_type)| (name.clone(), *boxed_type.clone()))
                .collect(),
            _ => vec![],
        };

        if version == JSON_OBJECT_VERSION_STRING {
            state.type_specific = TypeSpecificState::Json(JsonStateData {
                version: Some(version),
                typed_paths,
                ..JsonStateData::default()
            });
            return Ok(());
        }

        // Handle V1 and V2 (shared data formats)
        if version == JSON_OBJECT_VERSION_V1 || version == JSON_OBJECT_VERSION_V2 {
            return Self::read_prefix_shared_data(version, typed_paths, reader, state).await;
        }

        if version != JSON_OBJECT_VERSION_FLATTENED {
            return Err(Error::DeserializeError(format!(
                "Unsupported JSON serialization version {version}. Expected V1 (0), STRING (1), \
                 V2 (2), or FLATTENED (3)."
            )));
        }

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

        // Build and cache path segments for typed and dynamic paths
        let mut path_segments = BTreeMap::new();
        for (name, _) in &typed_paths {
            drop(
                path_segments
                    .insert(name.clone(), name.split('.').map(|s| s.to_string()).collect()),
            );
        }
        for name in &dynamic_path_names {
            drop(
                path_segments
                    .insert(name.clone(), name.split('.').map(|s| s.to_string()).collect()),
            );
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
            path_segments,
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
}

impl JsonDeserializer {
    /* sync prefix removed; async-only */
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
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

    #[tokio::test]
    async fn json_string_serialization_is_parsed() {
        let rows: [&[u8]; 2] = [br#"{"a":1}"#, br#"{"b":"x"}"#];

        let mut payload = Vec::new();
        payload.extend_from_slice(&JSON_OBJECT_VERSION_STRING.to_le_bytes());
        for row in &rows {
            payload.push(row.len() as u8);
            payload.extend_from_slice(row);
        }

        let mut reader = Cursor::new(payload);
        let mut state = DeserializerState::default();
        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        JsonDeserializer::read_prefix(&type_, &mut reader, &mut state).await.unwrap();
        let values =
            JsonDeserializer::read(&type_, &mut reader, rows.len(), &mut state).await.unwrap();

        {
            assert_eq!(values, vec![
                Value::Json(serde_json::json!({"a": 1})),
                Value::Json(serde_json::json!({"b": "x"})),
            ]);
        }
    }

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

    // Removed sync prefix tests.

    // Note: JSON sync roundtrip testing is handled by the integration test
    // in src/native/types/tests.rs (roundtrip_complex_types_sync)

    // V1/V2 Shared Data Map Tests

    #[tokio::test]
    async fn test_read_shared_data_map_empty() {
        // Test with 3 rows, all with 0 entries
        let mut payload = Vec::new();

        // Stream 1: ArraySizes (cumulative offsets, all 0)
        payload.extend_from_slice(&0u64.to_le_bytes()); // row 0: 0 entries
        payload.extend_from_slice(&0u64.to_le_bytes()); // row 1: 0 entries
        payload.extend_from_slice(&0u64.to_le_bytes()); // row 2: 0 entries

        // Stream 2: Keys (empty, 0 total entries)
        // Stream 3: Values (empty, 0 total entries)

        let mut reader = Cursor::new(payload);
        let result = JsonDeserializer::read_shared_data_map(&mut reader, 3).await.unwrap();

        assert_eq!(result.len(), 3);
        assert!(result[0].is_empty());
        assert!(result[1].is_empty());
        assert!(result[2].is_empty());
    }

    #[tokio::test]
    async fn test_read_shared_data_map_single_entry_per_row() {
        // Test with 2 rows, each with 1 entry
        let mut payload = Vec::new();

        // Stream 1: ArraySizes (cumulative offsets)
        payload.extend_from_slice(&1u64.to_le_bytes()); // row 0: 1 entry (cumulative: 1)
        payload.extend_from_slice(&2u64.to_le_bytes()); // row 1: 1 entry (cumulative: 2)

        // Stream 2: Keys (2 total keys)
        // Key 0: "path.a" (6 bytes)
        payload.push(6); // VarUInt length
        payload.extend_from_slice(b"path.a");
        // Key 1: "path.b" (6 bytes)
        payload.push(6); // VarUInt length
        payload.extend_from_slice(b"path.b");

        // Stream 3: Values (2 total values, binary encoded)
        // Value 0: type_byte=0x0a (Int64) + 42 as i64
        let mut value0 = vec![0x0a]; // Int64 type byte
        value0.extend_from_slice(&42i64.to_le_bytes());
        payload.push(value0.len() as u8); // VarUInt length
        payload.extend_from_slice(&value0);

        // Value 1: type_byte=0x15 (String) + "hello" (5 bytes)
        let mut value1 = vec![0x15]; // String type byte
        value1.push(5); // String length VarUInt
        value1.extend_from_slice(b"hello");
        payload.push(value1.len() as u8); // VarUInt length
        payload.extend_from_slice(&value1);

        let mut reader = Cursor::new(payload);
        let result = JsonDeserializer::read_shared_data_map(&mut reader, 2).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1);
        assert_eq!(result[1].len(), 1);

        assert!(result[0].contains_key("path.a"));
        assert!(result[1].contains_key("path.b"));

        // Verify binary value bytes
        let expected_value0: Vec<u8> = {
            let mut v = vec![0x0a];
            v.extend_from_slice(&42i64.to_le_bytes());
            v
        };
        assert_eq!(result[0].get("path.a").unwrap(), &expected_value0);
    }

    #[tokio::test]
    async fn test_read_shared_data_map_multiple_entries_per_row() {
        // Test with 2 rows: first has 2 entries, second has 3 entries
        let mut payload = Vec::new();

        // Stream 1: ArraySizes (cumulative offsets)
        payload.extend_from_slice(&2u64.to_le_bytes()); // row 0: 2 entries (cumulative: 2)
        payload.extend_from_slice(&5u64.to_le_bytes()); // row 1: 3 entries (cumulative: 5)

        // Stream 2: Keys (5 total keys)
        let keys = vec!["a", "b", "c", "d", "e"];
        for key in &keys {
            payload.push(key.len() as u8);
            payload.extend_from_slice(key.as_bytes());
        }

        // Stream 3: Values (5 total values, all Int64 for simplicity)
        for i in 0..5 {
            let mut value = vec![0x0a]; // Int64 type byte
            value.extend_from_slice(&(i as i64).to_le_bytes());
            payload.push(value.len() as u8);
            payload.extend_from_slice(&value);
        }

        let mut reader = Cursor::new(payload);
        let result = JsonDeserializer::read_shared_data_map(&mut reader, 2).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2); // First row: 2 entries
        assert_eq!(result[1].len(), 3); // Second row: 3 entries

        // Verify row 0 has keys "a" and "b"
        assert!(result[0].contains_key("a"));
        assert!(result[0].contains_key("b"));

        // Verify row 1 has keys "c", "d", and "e"
        assert!(result[1].contains_key("c"));
        assert!(result[1].contains_key("d"));
        assert!(result[1].contains_key("e"));
    }

    #[tokio::test]
    async fn test_read_shared_data_map_mixed_empty_and_data() {
        // Test with 4 rows: empty, data, empty, data
        let mut payload = Vec::new();

        // Stream 1: ArraySizes (cumulative offsets)
        payload.extend_from_slice(&0u64.to_le_bytes()); // row 0: 0 entries
        payload.extend_from_slice(&1u64.to_le_bytes()); // row 1: 1 entry (cumulative: 1)
        payload.extend_from_slice(&1u64.to_le_bytes()); // row 2: 0 entries
        payload.extend_from_slice(&3u64.to_le_bytes()); // row 3: 2 entries (cumulative: 3)

        // Stream 2: Keys (3 total)
        let keys = vec!["x", "y", "z"];
        for key in &keys {
            payload.push(key.len() as u8);
            payload.extend_from_slice(key.as_bytes());
        }

        // Stream 3: Values (3 total)
        for i in 0..3 {
            let mut value = vec![0x0a]; // Int64
            value.extend_from_slice(&(i as i64).to_le_bytes());
            payload.push(value.len() as u8);
            payload.extend_from_slice(&value);
        }

        let mut reader = Cursor::new(payload);
        let result = JsonDeserializer::read_shared_data_map(&mut reader, 4).await.unwrap();

        assert_eq!(result.len(), 4);
        assert_eq!(result[0].len(), 0); // row 0: empty
        assert_eq!(result[1].len(), 1); // row 1: 1 entry
        assert_eq!(result[2].len(), 0); // row 2: empty
        assert_eq!(result[3].len(), 2); // row 3: 2 entries

        assert!(result[1].contains_key("x"));
        assert!(result[3].contains_key("y"));
        assert!(result[3].contains_key("z"));
    }

    // V1/V2 End-to-end Deserialization Tests

    #[tokio::test]
    async fn test_json_v2_deserialization_simple() {
        // Test V2 format with 1 typed path and shared data
        // JSON structure: {"id": <int>, "name": <string in shared data>}
        let mut payload = Vec::new();

        // ObjectStructure:
        // - version = 2
        payload.extend_from_slice(&2u64.to_le_bytes());
        // - num_dynamic_paths = 0 (no dynamic paths declared in structure)
        payload.push(0);
        // - typed paths are implicit (alphabetically sorted)

        // Data for 2 rows:
        // Row 0: {"id": 1, "name": "Alice"}
        // Row 1: {"id": 2, "name": "Bob"}

        // Typed path "id" column (Int64):
        // No prefix needed for Int64
        // Values: [1, 2]
        payload.extend_from_slice(&1i64.to_le_bytes());
        payload.extend_from_slice(&2i64.to_le_bytes());

        // Shared data Map:
        // Stream 1: ArraySizes (cumulative offsets)
        payload.extend_from_slice(&1u64.to_le_bytes()); // row 0: 1 entry
        payload.extend_from_slice(&2u64.to_le_bytes()); // row 1: 1 entry

        // Stream 2: Keys
        payload.push(4); // "name" length
        payload.extend_from_slice(b"name");
        payload.push(4); // "name" length
        payload.extend_from_slice(b"name");

        // Stream 3: Values (binary encoded strings)
        // Row 0: String "Alice"
        let mut alice_value = vec![0x15]; // String type byte
        alice_value.push(5); // length
        alice_value.extend_from_slice(b"Alice");
        payload.push(alice_value.len() as u8);
        payload.extend_from_slice(&alice_value);

        // Row 1: String "Bob"
        let mut bob_value = vec![0x15]; // String type byte
        bob_value.push(3); // length
        bob_value.extend_from_slice(b"Bob");
        payload.push(bob_value.len() as u8);
        payload.extend_from_slice(&bob_value);

        let mut reader = Cursor::new(payload);
        let mut state = DeserializerState::default();

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![("id".to_string(), Box::new(Type::Int64))],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        JsonDeserializer::read_prefix(&type_, &mut reader, &mut state).await.unwrap();
        let values = JsonDeserializer::read(&type_, &mut reader, 2, &mut state).await.unwrap();

        assert_eq!(values.len(), 2);

        // Verify row 0
        if let Value::Json(json0) = &values[0] {
            assert_eq!(json0.get("id").unwrap(), &serde_json::json!(1));
            assert_eq!(json0.get("name").unwrap(), &serde_json::json!("Alice"));
        } else {
            panic!("Expected JSON value");
        }

        // Verify row 1
        if let Value::Json(json1) = &values[1] {
            assert_eq!(json1.get("id").unwrap(), &serde_json::json!(2));
            assert_eq!(json1.get("name").unwrap(), &serde_json::json!("Bob"));
        } else {
            panic!("Expected JSON value");
        }
    }

    #[tokio::test]
    async fn test_json_v1_deserialization_with_max_dynamic_paths() {
        // Test V1 format which has extra max_dynamic_paths field
        let mut payload = Vec::new();

        // ObjectStructure:
        // - version = 0 (V1)
        payload.extend_from_slice(&0u64.to_le_bytes());
        // - max_dynamic_paths = 100 (this field is skipped)
        payload.push(100);
        // - num_dynamic_paths = 0
        payload.push(0);

        // Data for 1 row: {"value": 42}
        // Typed path "value" column (Int64):
        payload.extend_from_slice(&42i64.to_le_bytes());

        // Shared data Map (empty):
        // Stream 1: ArraySizes
        payload.extend_from_slice(&0u64.to_le_bytes()); // row 0: 0 entries

        let mut reader = Cursor::new(payload);
        let mut state = DeserializerState::default();

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![("value".to_string(), Box::new(Type::Int64))],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        JsonDeserializer::read_prefix(&type_, &mut reader, &mut state).await.unwrap();
        let values = JsonDeserializer::read(&type_, &mut reader, 1, &mut state).await.unwrap();

        assert_eq!(values.len(), 1);

        if let Value::Json(json) = &values[0] {
            assert_eq!(json.get("value").unwrap(), &serde_json::json!(42));
        } else {
            panic!("Expected JSON value");
        }
    }

    #[tokio::test]
    async fn test_json_v2_nested_paths_in_shared_data() {
        // Test V2 with nested paths like "user.profile.age" in shared data
        let mut payload = Vec::new();

        // ObjectStructure V2:
        payload.extend_from_slice(&2u64.to_le_bytes());
        payload.push(0); // no dynamic paths

        // Data for 1 row: {"user": {"profile": {"age": 30}}}
        // Shared data Map:
        // Stream 1: ArraySizes
        payload.extend_from_slice(&1u64.to_le_bytes()); // 1 entry

        // Stream 2: Keys
        let key = "user.profile.age";
        payload.push(key.len() as u8);
        payload.extend_from_slice(key.as_bytes());

        // Stream 3: Values (Int64 30)
        let mut value = vec![0x0a]; // Int64 type byte
        value.extend_from_slice(&30i64.to_le_bytes());
        payload.push(value.len() as u8);
        payload.extend_from_slice(&value);

        let mut reader = Cursor::new(payload);
        let mut state = DeserializerState::default();

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        JsonDeserializer::read_prefix(&type_, &mut reader, &mut state).await.unwrap();
        let values = JsonDeserializer::read(&type_, &mut reader, 1, &mut state).await.unwrap();

        assert_eq!(values.len(), 1);

        if let Value::Json(json) = &values[0] {
            // Should reconstruct nested structure
            let user = json.get("user").unwrap().as_object().unwrap();
            let profile = user.get("profile").unwrap().as_object().unwrap();
            assert_eq!(profile.get("age").unwrap(), &serde_json::json!(30));
        } else {
            panic!("Expected JSON value");
        }
    }
}
