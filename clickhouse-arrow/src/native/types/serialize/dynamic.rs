use std::collections::HashMap;

use tokio::io::AsyncWriteExt;
use tracing::trace;

use crate::Result;
use crate::formats::{DynamicState, SerializerState, TypeSpecificState};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::ClickHouseNativeSerializer;
use crate::native::types::{Type, Value};

const DYNAMIC_VERSION: u64 = 3; // Always use v3 (flattened format)

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

/// Handles serialization of Dynamic types
/// Dynamic is internally represented as a Variant with different serialization versions
pub(crate) struct DynamicSerializer;

impl DynamicSerializer {
    /// Check if server supports Dynamic v3
    fn check_server_version(state: &SerializerState) -> Result<()> {
        if let Some((major, minor, _)) = state.server_version
            && (major < 25 || (major == 25 && minor < 6))
        {
            return Err(crate::Error::SerializeError(format!(
                "Dynamic type requires ClickHouse server version >= 25.6, got {major}.{minor}"
            )));
        }
        Ok(())
    }

    /// Get Dynamic serialization version - always v3
    fn get_version(_state: &SerializerState) -> u64 { DYNAMIC_VERSION }

    /// Build type registry from values
    fn build_type_registry(
        values: &[Value],
    ) -> (Vec<String>, HashMap<String, (usize, Type)>, usize) {
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
            let old = type_map.insert(type_name.clone(), (index, value_type));
            debug_assert!(old.is_none());
        }

        let total_types = type_names.len();
        (type_names, type_map, total_types)
    }

    /// Build discriminators and group rows by type
    fn build_discriminators_and_groups(
        values: &[Value],
        type_map: &HashMap<String, (usize, Type)>,
        total_types: usize,
    ) -> (Vec<u64>, HashMap<usize, Vec<usize>>) {
        let mut discriminators = Vec::with_capacity(values.len());
        let mut rows_by_type: HashMap<usize, Vec<usize>> = HashMap::new();

        for (row_idx, value) in values.iter().enumerate() {
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types in v3
                discriminators.push(total_types as u64);
            } else {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();
                let (type_idx, _) = &type_map[&type_name];
                discriminators.push(*type_idx as u64);
                rows_by_type.entry(*type_idx).or_default().push(row_idx);
            }
        }

        (discriminators, rows_by_type)
    }

    /// Write column data for each type
    async fn write_columns<W: ClickHouseWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        rows_by_type: &HashMap<usize, Vec<usize>>,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for (type_idx, type_name) in type_names.iter().enumerate() {
            let (_, typ) = &type_map[type_name];

            // Get values for this type (empty if no rows)
            let type_values: Vec<Value> = if let Some(row_indices) = rows_by_type.get(&type_idx) {
                row_indices.iter().map(|&row_idx| values[row_idx].clone()).collect()
            } else {
                vec![]
            };

            trace!("Writing column {} ({}) with {} values", type_idx, type_name, type_values.len());

            // Write the column data (even if empty)
            typ.serialize_column(type_values, writer, state).await?;
        }
        Ok(())
    }

    /// Write column data for each type (sync)
    fn write_columns_sync<W: ClickHouseBytesWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        rows_by_type: &HashMap<usize, Vec<usize>>,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for (type_idx, type_name) in type_names.iter().enumerate() {
            let (_, typ) = &type_map[type_name];

            // Get values for this type (empty if no rows)
            let type_values: Vec<Value> = if let Some(row_indices) = rows_by_type.get(&type_idx) {
                row_indices.iter().map(|&row_idx| values[row_idx].clone()).collect()
            } else {
                vec![]
            };

            // Write the column data (even if empty)
            typ.serialize_column_sync(type_values, writer, state)?;
        }
        Ok(())
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        _type: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Check server version support
        Self::check_server_version(state)?;

        let version = Self::get_version(state);
        trace!("Writing Dynamic prefix with version {}", version);
        writer.write_u64_le(version).await?;

        // Check if we have metadata from previous analysis
        if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
            trace!(
                "Dynamic state: {} types: {:?}",
                dynamic_state.total_types, dynamic_state.type_names
            );

            // v3 format: total_types, then type names, then nested prefixes
            writer.write_var_uint(dynamic_state.total_types).await?;

            // Write type names
            for type_name in &dynamic_state.type_names {
                writer.write_string(type_name).await?;
            }

            // Clone type_names and type_map to avoid borrowing issues
            let type_names = dynamic_state.type_names.clone();
            let type_map = dynamic_state.type_map.clone();

            // Write nested type prefixes
            for type_name in &type_names {
                let (_, typ) = &type_map[type_name];
                typ.serialize_prefix_async(writer, state).await?;
            }
        } else {
            return Err(crate::Error::SerializeError(
                "Dynamic serialization state not found. `analyze_values` must be called before \
                 `write_prefix`."
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Analyze values and return type metadata for use in `write_prefix`
    pub(crate) fn analyze_values(values: &[Value]) -> TypeSpecificState {
        let (type_names, type_map, total_types) = Self::build_type_registry(values);
        let state = DynamicState {
            version: None, // Will be set during write_prefix
            total_types: total_types as u64,
            type_names,
            type_map,
            types: vec![], // Will be populated if needed
        };
        TypeSpecificState::Dynamic(state)
    }

    pub(crate) async fn write<W: ClickHouseWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Get metadata from state
        let (type_names, type_map, total_types) =
            if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
                let total = usize::try_from(dynamic_state.total_types).unwrap_or(usize::MAX);
                (dynamic_state.type_names.clone(), dynamic_state.type_map.clone(), total)
            } else {
                return Err(crate::Error::SerializeError(
                    "Dynamic serialization state not found. `analyze_values` must be called \
                     before `write`."
                        .to_string(),
                ));
            };

        // v3 uses variable-sized discriminators
        // Build discriminators and count rows per type
        let (discriminators, rows_by_type) =
            Self::build_discriminators_and_groups(values, &type_map, total_types);

        // Write discriminators
        for &disc in &discriminators {
            write_discriminator!(async writer, disc, total_types);
        }

        // Write column data for each type
        Self::write_columns(&type_names, &type_map, &rows_by_type, values, writer, state).await
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Check server version support
        Self::check_server_version(state)?;

        let version = Self::get_version(state);
        writer.put_u64_le(version);

        // Check if we have metadata from previous analysis
        if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
            // v3 format: total_types, then type names, then nested prefixes
            writer.put_var_uint(dynamic_state.total_types)?;

            // Write type names
            for type_name in &dynamic_state.type_names {
                writer.put_string(type_name)?;
            }

            // Clone type_names and type_map to avoid borrowing issues
            let type_names = dynamic_state.type_names.clone();
            let type_map = dynamic_state.type_map.clone();

            // Write nested type prefixes
            for type_name in &type_names {
                let (_, typ) = &type_map[type_name];
                typ.serialize_prefix(writer, state);
            }
        } else {
            return Err(crate::Error::SerializeError(
                "Dynamic serialization state not found. `analyze_values` must be called before \
                 `write_prefix`."
                    .to_string(),
            ));
        }

        Ok(())
    }

    pub(crate) fn write_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Get metadata from state
        let (type_names, type_map, total_types) =
            if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
                let total = usize::try_from(dynamic_state.total_types).unwrap_or(usize::MAX);
                (dynamic_state.type_names.clone(), dynamic_state.type_map.clone(), total)
            } else {
                return Err(crate::Error::SerializeError(
                    "Dynamic serialization state not found. `analyze_values` must be called \
                     before `write`."
                        .to_string(),
                ));
            };

        // v3 uses variable-sized discriminators
        // Build discriminators and count rows per type
        let (discriminators, rows_by_type) =
            Self::build_discriminators_and_groups(values, &type_map, total_types);

        // Write discriminators
        for &disc in &discriminators {
            write_discriminator!(sync writer, disc, total_types);
        }

        // Write column data for each type
        Self::write_columns_sync(&type_names, &type_map, &rows_by_type, values, writer, state)
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use bytes::{Buf, BufMut};

    use super::*;
    use crate::io::{ClickHouseBytesRead, ClickHouseBytesWrite};

    // Helper function to create a basic serializer state with analyzed values
    fn create_test_state(values: &[Value]) -> SerializerState {
        let type_specific_state = DynamicSerializer::analyze_values(values);
        SerializerState { type_specific: type_specific_state, ..Default::default() }
    }

    // Assert that type names serialize correctly in round-trip
    fn assert_type_names_serialization(type_names: &[String]) {
        let mut buffer = Vec::new();

        // Write total count and type names
        buffer.put_var_uint(type_names.len() as u64).unwrap();
        for type_name in type_names {
            buffer.put_string(type_name).unwrap();
        }

        // Verify serialization round-trip
        let mut reader = &buffer[..];
        let count = reader.try_get_var_uint().unwrap();
        assert_eq!(count, type_names.len() as u64);
        for expected in type_names {
            let bytes = reader.try_get_string().unwrap();
            let actual = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    // Assert version compatibility behavior
    fn assert_version_compatibility(major: u64, minor: u64, patch: u64, should_succeed: bool) {
        let values = vec![Value::Int32(42), Value::String(b"test".to_vec())];
        let mut state = create_test_state(&values);
        state.server_version = Some((major, minor, patch));

        let mut buffer = Vec::new();
        let result = DynamicSerializer::write_prefix_sync(
            &Type::Dynamic { max_types: None },
            &mut buffer,
            &mut state,
        );

        if should_succeed {
            assert!(result.is_ok(), "Version {major}.{minor}.{patch} should succeed");
        } else {
            assert!(result.is_err(), "Version {major}.{minor}.{patch} should fail");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("requires ClickHouse server version >= 25.6")
            );
        }
    }

    // Assert discriminator size matches expectations for given total_types
    fn assert_discriminator_size(total_types: usize, expected_bytes: usize, description: &str) {
        let mut buffer = Vec::new();
        write_discriminator!(sync &mut buffer, 0, total_types);
        assert_eq!(
            buffer.len(),
            expected_bytes,
            "Failed for total_types={total_types} ({description})"
        );

        // Test both min and max discriminator values for this size
        buffer.clear();
        let max_disc = std::cmp::min(total_types - 1, match expected_bytes {
            1 => 255,
            2 => 65535,
            4 => 4_294_967_295,
            _ => total_types - 1,
        });
        write_discriminator!(sync &mut buffer, max_disc as u64, total_types);
        assert_eq!(
            buffer.len(),
            expected_bytes,
            "Max discriminator failed for total_types={total_types}"
        );
    }

    // Type name serialization tests
    #[test]
    fn test_type_names_basic_serialization() {
        let type_names =
            vec!["Array(Int32)".to_string(), "Date".to_string(), "Float32".to_string()];
        assert_type_names_serialization(&type_names);
    }

    #[test]
    fn test_type_registry_building_with_mixed_values() {
        let values = vec![
            Value::String(b"test".to_vec()),
            Value::Int32(42),
            Value::String(b"another".to_vec()),
            Value::Float64(std::f64::consts::PI),
            Value::Null,
            Value::Int32(99),
        ];
        let (reg_type_names, type_map, total_types) =
            DynamicSerializer::build_type_registry(&values);

        // Verify alphabetical ordering and correct indices
        assert_eq!(reg_type_names, vec!["Float64", "Int32", "String"]);
        assert_eq!(total_types, 3);
        assert_eq!(type_map["Float64"].0, 0);
        assert_eq!(type_map["Int32"].0, 1);
        assert_eq!(type_map["String"].0, 2);
    }

    #[test]
    fn test_type_registry_null_handling() {
        let values = vec![Value::Null, Value::Null, Value::Int32(42)];
        let (type_names, type_map, total_types) = DynamicSerializer::build_type_registry(&values);

        // Only Int32 should be in registry (nulls ignored)
        assert_eq!(type_names, vec!["Int32"]);
        assert_eq!(total_types, 1);
        assert_eq!(type_map["Int32"].0, 0);
    }

    // Value type guessing tests
    #[test]
    fn test_value_type_guessing_basic_types() {
        let test_cases = vec![
            (Value::Int32(42), "Int32"),
            (Value::String(b"hello".to_vec()), "String"),
            (Value::Float64(std::f64::consts::PI), "Float64"),
            (Value::UInt64(12345), "UInt64"),
            (Value::Float32(1.5), "Float32"),
        ];

        for (value, expected_type_name) in test_cases {
            let guessed_type = value.guess_type();
            let type_name = guessed_type.to_string();
            assert_eq!(type_name, expected_type_name, "For value {value:?}");
        }
    }

    // Dynamic v3 prefix serialization tests
    #[test]
    fn test_dynamic_v3_prefix_format() {
        let mut buffer = Vec::new();

        // Write v3 serialization version and type data
        buffer.put_u64_le(DYNAMIC_VERSION);
        buffer.put_var_uint(3).unwrap();

        // Write type names in alphabetical order
        let type_names = vec!["Float64", "Int32", "String"];
        for name in &type_names {
            buffer.put_string(name).unwrap();
        }

        // Verify round-trip deserialization
        let mut reader = &buffer[..];
        let version = reader.get_u64_le();
        assert_eq!(version, DYNAMIC_VERSION);
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 3);
        for expected in &type_names {
            let bytes = reader.try_get_string().unwrap();
            let actual = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    #[tokio::test]
    async fn test_dynamic_prefix_writing_integration() {
        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ];

        // Analyze values and write prefix
        let type_specific_state = DynamicSerializer::analyze_values(&values);
        let mut buffer = Vec::new();
        let mut state =
            SerializerState { type_specific: type_specific_state, ..Default::default() };
        DynamicSerializer::write_prefix(
            &Type::Dynamic { max_types: None },
            &mut buffer,
            &mut state,
        )
        .await
        .unwrap();

        // Verify version and type count
        let mut reader = &buffer[..];
        assert_eq!(reader.get_u64_le(), DYNAMIC_VERSION);
        assert_eq!(reader.try_get_var_uint().unwrap(), 3);
    }

    // Server version compatibility tests
    #[test]
    fn test_version_check_too_old_major() { assert_version_compatibility(24, 12, 0, false); }

    #[test]
    fn test_version_check_too_old_minor() {
        assert_version_compatibility(25, 1, 0, false);
        assert_version_compatibility(25, 5, 0, false);
    }

    #[test]
    fn test_version_check_minimum_supported() { assert_version_compatibility(25, 6, 0, true); }

    #[test]
    fn test_version_check_newer_supported() {
        assert_version_compatibility(25, 7, 0, true);
        assert_version_compatibility(26, 0, 0, true);
    }

    #[test]
    fn test_version_check_no_version_info() {
        let values = vec![Value::Int32(42), Value::String(b"test".to_vec())];
        let mut state = create_test_state(&values);
        state.server_version = None; // No version info should pass

        let mut buffer = Vec::new();
        let result = DynamicSerializer::write_prefix_sync(
            &Type::Dynamic { max_types: None },
            &mut buffer,
            &mut state,
        );
        assert!(result.is_ok(), "No version info should succeed");
    }

    // Discriminator size optimization tests
    #[test]
    fn test_discriminator_u8_range() {
        assert_discriminator_size(100, 1, "fits in u8");
        assert_discriminator_size(255, 1, "max u8");
    }

    #[test]
    fn test_discriminator_u16_range() {
        assert_discriminator_size(256, 2, "needs u16");
        assert_discriminator_size(65535, 2, "max u16");
    }

    #[test]
    fn test_discriminator_u32_range() {
        assert_discriminator_size(65536, 4, "needs u32");
        assert_discriminator_size(4_294_967_295, 4, "max u32");
    }

    #[test]
    fn test_discriminator_u64_range() {
        let total_types = 4_294_967_296_usize;
        let mut buffer = Vec::new();
        write_discriminator!(sync &mut buffer, 0, total_types);
        assert_eq!(buffer.len(), 8, "Should use u64 for very large total_types");
    }
}
