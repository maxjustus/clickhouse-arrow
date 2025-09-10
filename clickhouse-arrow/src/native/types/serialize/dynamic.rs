#![allow(clippy::approx_constant)]
use std::collections::HashMap;

use tokio::io::AsyncWriteExt;
use tracing::trace;

use crate::formats::{DynamicState, SerializerState, TypeSpecificState};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::ClickHouseNativeSerializer;
use crate::native::types::{Type, Value};
use crate::{Result, write_discriminator};

// Using FLATTENED format (version 3) for client compatibility
const DYNAMIC_VERSION_FLATTENED: u64 = 3;

/// Handles serialization of Dynamic types
/// Dynamic is internally represented as a Variant with different serialization versions
#[derive(Copy, Clone)]
pub struct DynamicSerializer;

impl DynamicSerializer {
    /// Write Dynamic column data only (used by JSON in write phase)
    pub(crate) async fn write_dynamic_data_async<W: ClickHouseWrite>(
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
        dynamic_state: TypeSpecificState,
    ) -> Result<()> {
        // Temporarily swap state to use provided Dynamic state
        let original_state = std::mem::replace(&mut state.type_specific, dynamic_state);

        // Write the actual data
        Self::write_internal_async(&Type::Dynamic { max_types: None }, values, writer, state)
            .await?;

        // Restore original state
        state.type_specific = original_state;
        Ok(())
    }

    /// Write Dynamic column data only - sync version
    pub(crate) fn write_dynamic_data_sync<W: ClickHouseBytesWrite>(
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
        dynamic_state: TypeSpecificState,
    ) -> Result<()> {
        // Temporarily swap state to use provided Dynamic state
        let original_state = std::mem::replace(&mut state.type_specific, dynamic_state);

        // Write the actual data
        Self::write_internal_sync(&Type::Dynamic { max_types: None }, values, writer, state)?;

        // Restore original state
        state.type_specific = original_state;
        Ok(())
    }

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

    /// Get Dynamic serialization version - always FLATTENED
    fn get_version(_state: &SerializerState) -> u64 { DYNAMIC_VERSION_FLATTENED }

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
    ) -> (Vec<u64>, HashMap<usize, Vec<Value>>) {
        let mut discriminators = Vec::with_capacity(values.len());
        let mut rows_by_type: HashMap<usize, Vec<Value>> = HashMap::new();

        for value in values {
            if matches!(value, Value::Null) {
                // NULL discriminator is total_types in v3
                discriminators.push(total_types as u64);
            } else {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();
                let (type_idx, _) = &type_map[&type_name];
                discriminators.push(*type_idx as u64);
                rows_by_type.entry(*type_idx).or_default().push(value.clone());
            }
        }

        (discriminators, rows_by_type)
    }

    /// Write column data for each type (async version)
    async fn write_columns_internal_async<W: ClickHouseWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        rows_by_type: &HashMap<usize, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for (type_idx, type_name) in type_names.iter().enumerate() {
            let (_, typ) = &type_map[type_name];

            // Get values for this type (empty if no rows)
            let type_values = rows_by_type.get(&type_idx).cloned().unwrap_or_default();

            trace!("Writing column {} ({}) with {} values", type_idx, type_name, type_values.len());

            // Write the column data (even if empty)
            typ.serialize_column(type_values, writer, state).await?;
        }
        Ok(())
    }

    /// Write column data for each type (sync version)
    fn write_columns_internal_sync<W: ClickHouseBytesWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        rows_by_type: &HashMap<usize, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for (type_idx, type_name) in type_names.iter().enumerate() {
            let (_, typ) = &type_map[type_name];

            // Get values for this type (empty if no rows)
            let type_values = rows_by_type.get(&type_idx).cloned().unwrap_or_default();

            // Write the column data (even if empty)
            typ.serialize_column_sync(type_values, writer, state)?;
        }
        Ok(())
    }

    /// Write complete Dynamic data (async version)
    pub(crate) async fn write_internal_async<W: ClickHouseWrite>(
        _: &Type,
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
        Self::write_columns_internal_async(&type_names, &type_map, &rows_by_type, writer, state)
            .await
    }

    /// Write complete Dynamic data (sync version)
    pub(crate) fn write_internal_sync<W: ClickHouseBytesWrite>(
        _: &Type,
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
        Self::write_columns_internal_sync(&type_names, &type_map, &rows_by_type, writer, state)
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
        // Always write version; JSON FLATTENED expects a per-path Dynamic header version.
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
    pub fn analyze_values(values: &[Value]) -> TypeSpecificState {
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

    #[allow(clippy::used_underscore_binding)]
    pub(crate) async fn write<W: ClickHouseWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        Self::write_internal_async(_type, values, writer, state).await
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
        // Always write version; JSON FLATTENED expects a per-path Dynamic header version.
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

    #[allow(clippy::used_underscore_binding)]
    pub(crate) fn write_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        Self::write_internal_sync(_type, values, writer, state)
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

    // Assert version compatibility behavior for both sync and async
    macro_rules! version_compatibility_test {
        ($name:ident, $major:expr, $minor:expr, $patch:expr, $should_succeed:expr) => {
            #[tokio::test]
            async fn $name() {
                println!("Testing scenario: {}", stringify!($name));
                let values = vec![Value::Int32(42), Value::String(b"test".to_vec())];
                let mut state = create_test_state(&values);
                state.server_version = Some(($major, $minor, $patch));

                // Test async
                let mut async_buffer = Vec::new();
                let async_result = DynamicSerializer::write_prefix(
                    &Type::Dynamic { max_types: None },
                    &mut async_buffer,
                    &mut state,
                )
                .await;

                // Test sync
                let mut sync_buffer = Vec::new();
                let sync_result = DynamicSerializer::write_prefix_sync(
                    &Type::Dynamic { max_types: None },
                    &mut sync_buffer,
                    &mut state,
                );

                if $should_succeed {
                    assert!(
                        async_result.is_ok(),
                        "Async version {}.{}.{} should succeed",
                        $major,
                        $minor,
                        $patch
                    );
                    assert!(
                        sync_result.is_ok(),
                        "Sync version {}.{}.{} should succeed",
                        $major,
                        $minor,
                        $patch
                    );
                    assert_eq!(async_buffer, sync_buffer, "Async and sync outputs should match");
                } else {
                    assert!(
                        async_result.is_err(),
                        "Async version {}.{}.{} should fail",
                        $major,
                        $minor,
                        $patch
                    );
                    assert!(
                        sync_result.is_err(),
                        "Sync version {}.{}.{} should fail",
                        $major,
                        $minor,
                        $patch
                    );
                    let async_err = async_result.unwrap_err().to_string();
                    let sync_err = sync_result.unwrap_err().to_string();
                    assert!(async_err.contains("requires ClickHouse server version >= 25.6"));
                    assert!(sync_err.contains("requires ClickHouse server version >= 25.6"));
                }
            }
        };
    }

    // Helper function to write discriminator in an async context with proper error handling
    async fn write_discriminator_async(
        buffer: &mut Vec<u8>,
        disc: u64,
        total_types: usize,
    ) -> Result<()> {
        write_discriminator!(async buffer, disc, total_types);
        Ok(())
    }

    // Assert discriminator size matches expectations for given total_types (sync and async)
    macro_rules! discriminator_test {
        ($name:ident, $total_types:expr, $expected_bytes:expr, $description:expr) => {
            #[tokio::test]
            async fn $name() {
                println!("Testing scenario: {}", stringify!($name));
                // Test sync discriminator
                let mut sync_buffer = Vec::new();
                write_discriminator!(sync &mut sync_buffer, 0, $total_types);
                assert_eq!(
                    sync_buffer.len(),
                    $expected_bytes,
                    "Sync failed for total_types={} ({})", $total_types, $description
                );

                // Test async discriminator
                let mut async_buffer = Vec::new();
                write_discriminator_async(&mut async_buffer, 0, $total_types).await.unwrap();
                assert_eq!(
                    async_buffer.len(),
                    $expected_bytes,
                    "Async failed for total_types={} ({})", $total_types, $description
                );

                assert_eq!(sync_buffer, async_buffer, "Sync and async discriminators should match");

                // Test both min and max discriminator values for this size
                let max_disc = std::cmp::min($total_types - 1, match $expected_bytes {
                    1 => 255,
                    2 => 65535,
                    4 => 4_294_967_295_usize,
                    _ => $total_types - 1,
                }) as u64;

                sync_buffer.clear();
                async_buffer.clear();

                write_discriminator!(sync &mut sync_buffer, max_disc, $total_types);
                write_discriminator_async(&mut async_buffer, max_disc, $total_types).await.unwrap();

                assert_eq!(
                    sync_buffer.len(),
                    $expected_bytes,
                    "Sync max discriminator failed for total_types={}", $total_types
                );
                assert_eq!(
                    async_buffer.len(),
                    $expected_bytes,
                    "Async max discriminator failed for total_types={}", $total_types
                );
                assert_eq!(sync_buffer, async_buffer, "Sync and async max discriminators should match");
            }
        };
    }

    // Create a unified test for prefix writing that tests both sync and async
    macro_rules! prefix_integration_test {
        ($name:ident, $values:expr, $expected_type_count:expr) => {
            #[tokio::test]
            async fn $name() {
                println!("Testing scenario: {}", stringify!($name));
                let values = $values;

                // Test async path
                let type_specific_state = DynamicSerializer::analyze_values(&values);
                let mut async_buffer = Vec::new();
                let mut async_state = SerializerState {
                    type_specific: type_specific_state.clone(),
                    ..Default::default()
                };
                DynamicSerializer::write_prefix(
                    &Type::Dynamic { max_types: None },
                    &mut async_buffer,
                    &mut async_state,
                )
                .await
                .unwrap();

                // Test sync path
                let mut sync_buffer = Vec::new();
                let mut sync_state =
                    SerializerState { type_specific: type_specific_state, ..Default::default() };
                DynamicSerializer::write_prefix_sync(
                    &Type::Dynamic { max_types: None },
                    &mut sync_buffer,
                    &mut sync_state,
                )
                .unwrap();

                // Verify both produce the same output
                assert_eq!(async_buffer, sync_buffer, "Async and sync outputs should match");

                // Verify version and type count
                let mut reader = &sync_buffer[..];
                assert_eq!(reader.get_u64_le(), DYNAMIC_VERSION_FLATTENED);
                assert_eq!(reader.try_get_var_uint().unwrap(), $expected_type_count);
            }
        };
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

        // Write FLATTENED serialization version and type data
        buffer.put_u64_le(DYNAMIC_VERSION_FLATTENED);
        buffer.put_var_uint(3).unwrap();

        // Write type names in alphabetical order
        let type_names = vec!["Float64", "Int32", "String"];
        for name in &type_names {
            buffer.put_string(name).unwrap();
        }

        // Verify round-trip deserialization
        let mut reader = &buffer[..];
        let version = reader.get_u64_le();
        assert_eq!(version, DYNAMIC_VERSION_FLATTENED);
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 3);
        for expected in &type_names {
            let bytes = reader.try_get_string().unwrap();
            let actual = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    // Use the new unified prefix integration test
    prefix_integration_test!(
        test_dynamic_prefix_writing_integration,
        vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ],
        3
    );

    // Server version compatibility tests using new macro
    version_compatibility_test!(test_version_check_too_old_major, 24, 12, 0, false);
    version_compatibility_test!(test_version_check_too_old_minor_25_1, 25, 1, 0, false);
    version_compatibility_test!(test_version_check_too_old_minor_25_5, 25, 5, 0, false);
    version_compatibility_test!(test_version_check_minimum_supported, 25, 6, 0, true);
    version_compatibility_test!(test_version_check_newer_supported_25_7, 25, 7, 0, true);
    version_compatibility_test!(test_version_check_newer_supported_26_0, 26, 0, 0, true);

    #[tokio::test]
    async fn test_version_check_no_version_info() {
        let values = vec![Value::Int32(42), Value::String(b"test".to_vec())];
        let mut state = create_test_state(&values);
        state.server_version = None; // No version info should pass

        // Test async
        let mut async_buffer = Vec::new();
        let async_result = DynamicSerializer::write_prefix(
            &Type::Dynamic { max_types: None },
            &mut async_buffer,
            &mut state,
        )
        .await;
        assert!(async_result.is_ok(), "Async: No version info should succeed");

        // Test sync
        let mut sync_buffer = Vec::new();
        let sync_result = DynamicSerializer::write_prefix_sync(
            &Type::Dynamic { max_types: None },
            &mut sync_buffer,
            &mut state,
        );
        assert!(sync_result.is_ok(), "Sync: No version info should succeed");

        assert_eq!(async_buffer, sync_buffer, "Async and sync outputs should match");
    }

    // Discriminator size tests using new macro
    discriminator_test!(test_discriminator_u8_small, 100, 1, "fits in u8");
    discriminator_test!(test_discriminator_u8_max, 255, 1, "max u8");
    discriminator_test!(test_discriminator_u16_min, 256, 2, "needs u16");
    discriminator_test!(test_discriminator_u16_max, 65535, 2, "max u16");
    discriminator_test!(test_discriminator_u32_min, 65536, 4, "needs u32");
    discriminator_test!(test_discriminator_u32_max, 4_294_967_295_usize, 4, "max u32");

    #[tokio::test]
    async fn test_discriminator_u64_range() {
        let total_types = 4_294_967_296_usize;

        // Test sync
        let mut sync_buffer = Vec::new();
        write_discriminator!(sync &mut sync_buffer, 0, total_types);
        assert_eq!(sync_buffer.len(), 8, "Sync: Should use u64 for very large total_types");

        // Test async
        let mut async_buffer = Vec::new();
        write_discriminator_async(&mut async_buffer, 0, total_types).await.unwrap();
        assert_eq!(async_buffer.len(), 8, "Async: Should use u64 for very large total_types");

        assert_eq!(sync_buffer, async_buffer, "Sync and async u64 discriminators should match");
    }

    // Tests for nested and heterogeneous arrays

    #[test]
    fn test_homogeneous_nested_arrays() {
        // Test that homogeneous nested arrays work correctly
        let values = vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![
                Value::Array(vec![Value::String(b"a".to_vec()), Value::String(b"b".to_vec())]),
                Value::Array(vec![Value::String(b"c".to_vec()), Value::String(b"d".to_vec())]),
            ]),
            Value::Tuple(vec![
                Value::String(b"name".to_vec()),
                Value::Array(vec![Value::Float64(1.0), Value::Float64(2.0)]),
            ]),
        ];

        let state = DynamicSerializer::analyze_values(&values);

        if let TypeSpecificState::Dynamic(dynamic_state) = state {
            println!("Detected types:");
            for t in &dynamic_state.type_names {
                println!("  - {t}");
            }
            assert!(dynamic_state.type_names.contains(&"Array(Int32)".to_string()));
            assert!(dynamic_state.type_names.contains(&"Array(Array(String))".to_string()));
            assert!(
                dynamic_state.type_names.contains(&"Tuple(String, Array(Float64))".to_string())
            );
        } else {
            panic!("Expected Dynamic state");
        }
    }

    #[test]
    fn test_heterogeneous_detection_works() {
        // Test that guess_type() now correctly detects heterogeneous arrays
        let mixed_array = Value::Array(vec![
            Value::Int32(1),
            Value::String(b"mixed".to_vec()),
            Value::Float64(3.14),
        ]);

        let guessed = mixed_array.guess_type();
        // Now correctly detects heterogeneous array and wraps in Variant!
        assert_eq!(guessed.to_string(), "Array(Variant(Float64, Int32, String))");
    }

    #[test]
    fn test_evil_heterogeneous_nested_arrays() {
        // The evil case: deeply nested heterogeneous arrays
        let values = vec![
            // Heterogeneous at top level
            Value::Array(vec![
                Value::Int32(42),
                Value::String(b"mixed".to_vec()),
                Value::Float64(3.14),
            ]),
            // Even more evil: nested heterogeneous
            Value::Array(vec![
                Value::Int32(1),
                Value::String(b"level1".to_vec()),
                Value::Array(vec![
                    Value::Float64(2.71),
                    Value::Null,
                    Value::Array(vec![Value::Int8(99), Value::String(b"deep".to_vec())]),
                ]),
            ]),
        ];

        // This should NOT panic when heterogeneous support is implemented
        let state = DynamicSerializer::analyze_values(&values);

        if let TypeSpecificState::Dynamic(dynamic_state) = state {
            println!("Evil test - detected types:");
            for t in &dynamic_state.type_names {
                println!("  - {t}");
            }
            // When working, should detect Variant-wrapped types
            let has_variant = dynamic_state.type_names.iter().any(|t| t.contains("Variant"));
            assert!(has_variant, "Should detect heterogeneous arrays and wrap in Variant");
        } else {
            panic!("Expected Dynamic state");
        }
    }

    #[test]
    fn test_heterogeneous_array_guess_type() {
        let evil = Value::Array(vec![
            Value::Int32(42),
            Value::String(b"chaos".to_vec()),
            Value::Array(vec![Value::Float64(3.14), Value::Null]),
        ]);

        let guessed = evil.guess_type();
        let type_string = guessed.to_string();

        // Should detect and wrap heterogeneous arrays in Variant
        assert!(type_string.contains("Variant"), "Expected Variant wrapper, got: {type_string}");
    }
}
