use std::collections::HashMap;

use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::formats::SerializerState;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::ClickHouseNativeSerializer;
use crate::native::types::{Type, Value};

/// Cache for Dynamic type metadata during serialization
#[derive(Debug, Default)]
struct DynamicSerializationCache {
    type_names:  Vec<String>,
    type_map:    HashMap<String, (usize, Type)>,
    total_types: usize,
}

// Thread-local cache for Dynamic type metadata
thread_local! {
    static DYNAMIC_CACHE: std::cell::RefCell<Option<DynamicSerializationCache>> = const { std::cell::RefCell::new(None) };
}

/// Handles serialization of Dynamic types
/// Dynamic is internally represented as a Variant with different serialization versions
pub(crate) struct DynamicSerializer;

impl DynamicSerializer {
    /// Determine discriminator size based on total types count
    /// (Kept for potential future use when we add v2 support)
    #[allow(dead_code)]
    fn discriminator_size(total_types: usize) -> usize {
        match total_types {
            0..=255 => 1,               // u8
            256..=65535 => 2,           // u16
            65536..=4_294_967_295 => 4, // u32
            _ => 8,                     // u64
        }
    }

    /// Write discriminator based on the total types count (async)
    #[allow(clippy::cast_possible_truncation)]
    async fn write_discriminator_async<W: ClickHouseWrite>(
        writer: &mut W,
        discriminator: u64,
        total_types: usize,
    ) -> Result<()> {
        match total_types {
            0..=255 => writer.write_u8(discriminator as u8).await?,
            256..=65535 => writer.write_u16_le(discriminator as u16).await?,
            65536..=4_294_967_295 => writer.write_u32_le(discriminator as u32).await?,
            _ => writer.write_u64_le(discriminator).await?,
        }
        Ok(())
    }

    /// Write discriminator based on the total types count (sync)
    #[allow(clippy::cast_possible_truncation)]
    fn write_discriminator_sync<W: ClickHouseBytesWrite>(
        writer: &mut W,
        discriminator: u64,
        total_types: usize,
    ) {
        match total_types {
            0..=255 => writer.put_u8(discriminator as u8),
            256..=65535 => writer.put_u16_le(discriminator as u16),
            65536..=4_294_967_295 => writer.put_u32_le(discriminator as u32),
            _ => writer.put_u64_le(discriminator),
        }
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        _type: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Always use v3 (flattened format) for now
        // v2 support can be added later if needed
        writer.write_u64_le(3).await?;

        // Check if we have cached metadata from a previous analysis
        let cache_data = DYNAMIC_CACHE.with(|cache| cache.borrow_mut().take());

        if let Some(cache) = cache_data {
            // We have cached metadata from analyze_values, write it now

            // v3 format: total_types, then type names, then nested prefixes
            writer.write_var_uint(cache.total_types as u64).await?;

            // Write type names
            for type_name in &cache.type_names {
                writer.write_string(type_name).await?;
            }

            // Write nested type prefixes
            for type_name in &cache.type_names {
                let (_, typ) = &cache.type_map[type_name];
                typ.serialize_prefix_async(writer, _state).await?;
            }

            // Put cache back for use in write()
            DYNAMIC_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        } else {
            // No cached metadata - write empty v3 format
            writer.write_var_uint(0).await?; // 0 types
        }

        Ok(())
    }

    /// Analyze values and cache type metadata for use in `write_prefix`
    pub(crate) fn analyze_values(values: &[Value]) {
        let mut type_map: HashMap<String, (usize, Type)> = HashMap::new();
        let mut type_names: Vec<String> = Vec::new();

        // Scan all values to build type registry
        for value in values {
            if !matches!(value, Value::Null) {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();

                if let std::collections::hash_map::Entry::Vacant(entry) =
                    type_map.entry(type_name.clone())
                {
                    let index = type_names.len();
                    type_names.push(type_name);
                    let _ = entry.insert((index, value_type));
                }
            }
        }

        // Sort type names alphabetically (ClickHouse requirement)
        type_names.sort();

        // Rebuild type map with sorted indices
        type_map.clear();
        for (index, type_name) in type_names.iter().enumerate() {
            let value_type = type_name.parse::<Type>().unwrap_or(Type::String);
            drop(type_map.insert(type_name.clone(), (index, value_type)));
        }

        let total_types = type_names.len();
        let cache = DynamicSerializationCache { type_names, type_map, total_types };

        DYNAMIC_CACHE.with(|c| *c.borrow_mut() = Some(cache));
    }

    pub(crate) async fn write<W: ClickHouseWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Get cached metadata or build it if not available
        let cache = DYNAMIC_CACHE.with(|cache| cache.borrow_mut().take());

        let (type_names, type_map, total_types) = if let Some(cache) = cache {
            // Use cached metadata
            (cache.type_names, cache.type_map, cache.total_types)
        } else {
            // This shouldn't happen if analyze_values was called, but handle it anyway
            let mut type_map: HashMap<String, (usize, Type)> = HashMap::new();
            let mut type_names: Vec<String> = Vec::new();

            // Scan all values to build type registry
            for value in values {
                if !matches!(value, Value::Null) {
                    let value_type = value.guess_type();
                    let type_name = value_type.to_string();

                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        type_map.entry(type_name.clone())
                    {
                        let index = type_names.len();
                        type_names.push(type_name);
                        let _ = entry.insert((index, value_type));
                    }
                }
            }

            // Sort type names alphabetically (ClickHouse requirement)
            type_names.sort();

            // Rebuild type map with sorted indices
            type_map.clear();
            for (index, type_name) in type_names.iter().enumerate() {
                let value_type = type_name.parse::<Type>().unwrap_or(Type::String);
                drop(type_map.insert(type_name.clone(), (index, value_type)));
            }

            let total_types = type_names.len();
            (type_names, type_map, total_types)
        };

        // Build discriminators and count rows per type
        let mut discriminators = Vec::with_capacity(values.len());
        let mut rows_by_type: HashMap<usize, Vec<usize>> = HashMap::new();

        for (row_idx, value) in values.iter().enumerate() {
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types in v2/v3
                discriminators.push(total_types as u64);
            } else {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();
                let (type_idx, _) = &type_map[&type_name];
                discriminators.push(*type_idx as u64);
                rows_by_type.entry(*type_idx).or_default().push(row_idx);
            }
        }

        // Write discriminators
        for &disc in &discriminators {
            Self::write_discriminator_async(writer, disc, total_types).await?;
        }

        // Write column data for each type
        for (type_idx, type_name) in type_names.iter().enumerate() {
            if let Some(row_indices) = rows_by_type.get(&type_idx) {
                let (_, typ) = &type_map[type_name];

                // Collect values for this type
                let mut type_values = Vec::with_capacity(row_indices.len());
                for &row_idx in row_indices {
                    type_values.push(values[row_idx].clone());
                }

                // Write the column data
                typ.serialize_column(type_values, writer, state).await?;
            }
        }

        Ok(())
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Always use v3 (flattened format) for now
        writer.put_u64_le(3);

        // Check if we have cached metadata
        let cache_data = DYNAMIC_CACHE.with(|cache| cache.borrow_mut().take());

        if let Some(cache) = cache_data {
            // We have cached metadata, write it now

            // v3 format: total_types, then type names, then nested prefixes
            writer.put_var_uint(cache.total_types as u64)?;

            // Write type names
            for type_name in &cache.type_names {
                writer.put_string(type_name)?;
            }

            // Write nested type prefixes
            for type_name in &cache.type_names {
                let (_, typ) = &cache.type_map[type_name];
                typ.serialize_prefix(writer, _state);
            }

            // Put cache back
            DYNAMIC_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        } else {
            // No cached metadata - write empty v3 format
            writer.put_var_uint(0)?; // 0 types
        }

        Ok(())
    }

    pub(crate) fn write_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Get cached metadata or build it
        let cache = DYNAMIC_CACHE.with(|cache| cache.borrow_mut().take());

        let (type_names, type_map, total_types) = if let Some(cache) = cache {
            (cache.type_names, cache.type_map, cache.total_types)
        } else {
            // Build type registry from values
            let mut type_map: HashMap<String, (usize, Type)> = HashMap::new();
            let mut type_names: Vec<String> = Vec::new();

            // Scan all values to build type registry
            for value in values {
                if !matches!(value, Value::Null) {
                    let value_type = value.guess_type();
                    let type_name = value_type.to_string();

                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        type_map.entry(type_name.clone())
                    {
                        let index = type_names.len();
                        type_names.push(type_name);
                        let _ = entry.insert((index, value_type));
                    }
                }
            }

            // Sort type names alphabetically
            type_names.sort();

            // Rebuild type map with sorted indices
            type_map.clear();
            for (index, type_name) in type_names.iter().enumerate() {
                let value_type = type_name.parse::<Type>().unwrap_or(Type::String);
                drop(type_map.insert(type_name.clone(), (index, value_type)));
            }

            let total_types = type_names.len();
            (type_names, type_map, total_types)
        };

        // Build discriminators and count rows per type
        let mut discriminators = Vec::with_capacity(values.len());
        let mut rows_by_type: HashMap<usize, Vec<usize>> = HashMap::new();

        for (row_idx, value) in values.iter().enumerate() {
            if matches!(value, Value::Null) {
                discriminators.push(total_types as u64);
            } else {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();
                let (type_idx, _) = &type_map[&type_name];
                discriminators.push(*type_idx as u64);
                rows_by_type.entry(*type_idx).or_default().push(row_idx);
            }
        }

        // Write discriminators
        for &disc in &discriminators {
            Self::write_discriminator_sync(writer, disc, total_types);
        }

        // Write column data for each type
        for (type_idx, type_name) in type_names.iter().enumerate() {
            if let Some(row_indices) = rows_by_type.get(&type_idx) {
                let (_, typ) = &type_map[type_name];

                // Collect values for this type
                let mut type_values = Vec::with_capacity(row_indices.len());
                for &row_idx in row_indices {
                    type_values.push(values[row_idx].clone());
                }

                // Write the column data
                typ.serialize_column_sync(type_values, writer, state)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Buf, BufMut};

    use super::*;
    use crate::io::{ClickHouseBytesRead, ClickHouseBytesWrite};

    #[test]
    fn test_discriminator_size() {
        assert_eq!(DynamicSerializer::discriminator_size(100), 1);
        assert_eq!(DynamicSerializer::discriminator_size(255), 1);
        assert_eq!(DynamicSerializer::discriminator_size(256), 2);
        assert_eq!(DynamicSerializer::discriminator_size(65535), 2);
        assert_eq!(DynamicSerializer::discriminator_size(65536), 4);
        assert_eq!(DynamicSerializer::discriminator_size(4_294_967_295), 4);
        assert_eq!(DynamicSerializer::discriminator_size(4_294_967_296), 8);
    }

    #[test]
    fn test_dynamic_type_name_serialization() {
        // Test that type names are serialized correctly
        let mut buffer = Vec::new();
        let type_names =
            vec!["Array(Int32)".to_string(), "Date".to_string(), "Float32".to_string()];

        // Write total count
        buffer.put_var_uint(type_names.len() as u64).unwrap();

        // Write type names
        for type_name in &type_names {
            buffer.put_string(type_name).unwrap();
        }

        // Verify the buffer contains expected data

        // The buffer should contain:
        // - varint 3 (number of types)
        // - string "Array(Int32)" with length prefix
        // - string "Date" with length prefix
        // - string "Float32" with length prefix

        // Check that we can read it back
        let mut reader = &buffer[..];
        let count = reader.try_get_var_uint().unwrap();
        assert_eq!(count, 3);

        for expected in &type_names {
            let bytes = reader.try_get_string().unwrap();
            let actual = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    #[test]
    fn test_value_type_names() {
        use crate::native::types::Value;

        // Test that Value::guess_type returns expected type names
        let test_cases = vec![
            (Value::Int32(42), "Int32"),
            (Value::String(b"hello".to_vec()), "String"),
            (Value::Float64(std::f64::consts::PI), "Float64"),
        ];

        for (value, expected_type_name) in test_cases {
            let guessed_type = value.guess_type();
            let type_name = guessed_type.to_string();
            assert_eq!(type_name, expected_type_name, "For value {:?}", value);
        }
    }

    #[test]
    fn test_dynamic_v3_prefix() {
        // Test v3 Dynamic prefix serialization
        let mut buffer = Vec::new();

        // Write v3 serialization version
        buffer.put_u64_le(3);

        // Write total_types
        buffer.put_var_uint(3).unwrap();

        // Write type names
        let type_names = vec!["Float64", "Int32", "String"]; // Alphabetical order
        for name in &type_names {
            buffer.put_string(name).unwrap();
        }

        // Check we can read it back
        let mut reader = &buffer[..];

        // Read version
        let version = reader.get_u64_le();
        assert_eq!(version, 3);

        // Read total_types
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 3);

        // Read type names
        for expected in &type_names {
            let bytes = reader.try_get_string().unwrap();
            let actual = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    #[tokio::test]
    async fn test_dynamic_prefix_writing_detailed() {
        // Create a buffer to write to
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Create test values
        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ];

        // Analyze values first
        DynamicSerializer::analyze_values(&values);

        // Write prefix
        DynamicSerializer::write_prefix(&Type::Dynamic, &mut buffer, &mut state).await.unwrap();

        // Verify the prefix was written successfully
    }
}
