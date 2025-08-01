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
            assert_eq!(type_name, expected_type_name, "For value {value:?}");
        }
    }

    #[test]
    fn test_dynamic_v3_prefix() {
        // Test v3 Dynamic prefix serialization
        let mut buffer = Vec::new();

        // Write v3 serialization version
        buffer.put_u64_le(DYNAMIC_VERSION);

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
        assert_eq!(version, DYNAMIC_VERSION);

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
        // Create test values
        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ];

        // Analyze values first
        let type_specific_state = DynamicSerializer::analyze_values(&values);

        // Write prefix
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

    #[test]
    fn test_build_type_registry() {
        let values = vec![
            Value::String(b"test".to_vec()),
            Value::Int32(42),
            Value::String(b"another".to_vec()),
            Value::Float64(std::f64::consts::PI),
            Value::Null,
            Value::Int32(99),
        ];

        let (type_names, type_map, total_types) = DynamicSerializer::build_type_registry(&values);

        // Check alphabetical ordering
        assert_eq!(type_names, vec!["Float64", "Int32", "String"]);
        assert_eq!(total_types, 3);

        // Check type indices
        assert_eq!(type_map["Float64"].0, 0);
        assert_eq!(type_map["Int32"].0, 1);
        assert_eq!(type_map["String"].0, 2);
    }

    #[test]
    fn test_version_check() {
        // Test that version check works correctly
        let mut state = SerializerState::default();
        let values = vec![Value::Int32(42), Value::String(b"test".to_vec())];

        // Analyze values to set up state
        state.type_specific = DynamicSerializer::analyze_values(&values);

        // Test with server version < 25.6
        state.server_version = Some((25, 1, 0));
        let mut buffer = Vec::new();
        let result = DynamicSerializer::write_prefix_sync(
            &Type::Dynamic { max_types: None },
            &mut buffer,
            &mut state,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("requires ClickHouse server version >= 25.6")
        );

        // Test with server version >= 25.6
        state.server_version = Some((25, 6, 0));
        buffer.clear();
        let result = DynamicSerializer::write_prefix_sync(
            &Type::Dynamic { max_types: None },
            &mut buffer,
            &mut state,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_discriminator_writing() {
        // Test that discriminator size is chosen correctly
        let test_cases: Vec<(usize, usize)> = vec![
            (100, 1),   // fits in u8
            (255, 1),   // max u8
            (256, 2),   // needs u16
            (65535, 2), // max u16
            (65536, 4), // needs u32
        ];

        for (total_types, expected_bytes) in test_cases {
            let mut buffer = Vec::new();
            write_discriminator!(sync &mut buffer, 0, total_types);
            assert_eq!(buffer.len(), expected_bytes, "Failed for total_types={total_types}");
        }
    }
}
