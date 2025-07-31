use std::collections::HashMap;

use tokio::io::AsyncWriteExt;
use tracing::{trace, warn};

use crate::Result;
use crate::formats::{DynamicState, SerializerState, TypeSpecificState};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::ClickHouseNativeSerializer;
use crate::native::types::{Type, Value};

const DYNAMIC_VERSION: u64 = 3; // Always use v3 (flattened format)
const DYNAMIC_VERSION_V2: u64 = 2; // V2 format for servers 24.11-25.5
const DYNAMIC_VERSION_V1: u64 = 1; // V1 format for servers < 24.11
const DEFAULT_MAX_DYNAMIC_TYPES: u64 = 32; // Default max types for v1

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
    /// Get Dynamic serialization version based on server version
    fn get_version(state: &SerializerState) -> u64 {
        if let Some((major, minor, _)) = state.server_version {
            let version = match (major, minor) {
                (maj, _) if maj < 24 => DYNAMIC_VERSION_V1,
                (24, min) if min < 11 => DYNAMIC_VERSION_V1,
                (24, min) if min >= 11 => DYNAMIC_VERSION_V2,
                (25, min) if min < 6 => DYNAMIC_VERSION_V2,
                _ => DYNAMIC_VERSION, // v3 for 25.6+
            };
            trace!("Dynamic version detection: server {}.{} -> format v{}", major, minor, version);
            version
        } else {
            warn!("No server version available, defaulting to Dynamic v3");
            DYNAMIC_VERSION // Default to v3 if version unknown (for testing)
        }
    }


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

    /// Build discriminators for v1/v2 (8-bit discriminators, NULL=255)
    fn build_discriminators_v2(
        values: &[Value],
        type_map: &HashMap<String, (usize, Type)>,
    ) -> Vec<u8> {
        let mut discriminators = Vec::with_capacity(values.len());

        for value in values.iter() {
            if matches!(value, Value::Null) {
                // NULL discriminator is 255 in v1/v2
                discriminators.push(255);
            } else {
                let value_type = value.guess_type();
                let type_name = value_type.to_string();
                let (type_idx, _) = &type_map[&type_name];
                discriminators.push(*type_idx as u8);
            }
        }

        discriminators
    }

    /// Write variant data for v1/v2 using 8-bit discriminators (async)
    async fn write_variant_data_v2<W: ClickHouseWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        trace!("Writing variant data v2: {} values, {} types", values.len(), type_names.len());
        
        // Write variant discriminator mode (0 = BASIC mode)
        writer.write_var_uint(0).await?;
        trace!("Wrote variant discriminator mode: 0 (BASIC)");
        
        // Write 8-bit discriminators
        let discriminators = Self::build_discriminators_v2(values, type_map);
        trace!("Writing {} discriminators: {:?}", discriminators.len(), discriminators);
        for disc in &discriminators {
            writer.write_u8(*disc).await?;
        }

        // Group rows by type
        let mut rows_by_type: HashMap<usize, Vec<usize>> = HashMap::new();
        for (row_idx, disc) in discriminators.iter().enumerate() {
            if *disc != 255 {
                rows_by_type.entry(*disc as usize).or_default().push(row_idx);
            }
        }
        trace!("Rows by type: {:?}", rows_by_type);

        // Write column data for each type
        Self::write_columns(type_names, type_map, &rows_by_type, values, writer, state).await
    }

    /// Write variant data for v1/v2 using 8-bit discriminators (sync)
    fn write_variant_data_v2_sync<W: ClickHouseBytesWrite>(
        type_names: &[String],
        type_map: &HashMap<String, (usize, Type)>,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Write variant discriminator mode (0 = BASIC mode)
        writer.put_var_uint(0)?;
        
        // Write 8-bit discriminators
        let discriminators = Self::build_discriminators_v2(values, type_map);
        for disc in &discriminators {
            writer.put_u8(*disc);
        }

        // Group rows by type
        let mut rows_by_type: HashMap<usize, Vec<usize>> = HashMap::new();
        for (row_idx, disc) in discriminators.iter().enumerate() {
            if *disc != 255 {
                rows_by_type.entry(*disc as usize).or_default().push(row_idx);
            }
        }

        // Write column data for each type
        Self::write_columns_sync(type_names, type_map, &rows_by_type, values, writer, state)
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
        let version = Self::get_version(state);
        trace!("Writing Dynamic prefix with version {}", version);
        writer.write_u64_le(version).await?;

        // Check if we have metadata from previous analysis
        if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
            trace!("Dynamic state: {} types: {:?}", dynamic_state.total_types, dynamic_state.type_names);
            match version {
                DYNAMIC_VERSION_V1 => {
                    // v1 format: max_dynamic_types, total_types, then type names as strings
                    trace!("Writing v1 format: max_types={}, total_types={}", DEFAULT_MAX_DYNAMIC_TYPES, dynamic_state.total_types);
                    writer.write_var_uint(DEFAULT_MAX_DYNAMIC_TYPES).await?;
                    writer.write_var_uint(dynamic_state.total_types).await?;
                    
                    // Write type names as strings (not DataType format)
                    for type_name in &dynamic_state.type_names {
                        writer.write_string(type_name).await?;
                    }
                    
                    // Write variant serialization version (always 0)
                    writer.write_u64_le(0).await?;
                    
                    // Clone to avoid borrowing issues
                    let type_names = dynamic_state.type_names.clone();
                    let type_map = dynamic_state.type_map.clone();
                    
                    // Write nested type prefixes
                    for type_name in &type_names {
                        let (_, typ) = &type_map[type_name];
                        typ.serialize_prefix_async(writer, state).await?;
                    }
                }
                DYNAMIC_VERSION_V2 => {
                    // v2 format: total_types, then type names as strings
                    trace!("Writing v2 format: total_types={}", dynamic_state.total_types);
                    writer.write_var_uint(dynamic_state.total_types).await?;
                    
                    // Write type names as strings (not DataType format)
                    for type_name in &dynamic_state.type_names {
                        writer.write_string(type_name).await?;
                    }
                    
                    // Write variant serialization version (always 0)
                    writer.write_u64_le(0).await?;
                    
                    // Clone to avoid borrowing issues
                    let type_names = dynamic_state.type_names.clone();
                    let type_map = dynamic_state.type_map.clone();
                    
                    // Write nested type prefixes
                    for type_name in &type_names {
                        let (_, typ) = &type_map[type_name];
                        typ.serialize_prefix_async(writer, state).await?;
                    }
                }
                DYNAMIC_VERSION => {
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
                }
                _ => {
                    return Err(crate::Error::SerializeError(
                        format!("Unsupported Dynamic version: {version}")
                    ));
                }
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

        let version = Self::get_version(state);
        match version {
            DYNAMIC_VERSION_V1 | DYNAMIC_VERSION_V2 => {
                // v1 and v2 use the same variant data format with 8-bit discriminators
                Self::write_variant_data_v2(&type_names, &type_map, values, writer, state).await
            }
            DYNAMIC_VERSION => {
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
            _ => Err(crate::Error::SerializeError(
                format!("Unsupported Dynamic version: {version}")
            ))
        }
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        _type: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let version = Self::get_version(state);
        writer.put_u64_le(version);

        // Check if we have metadata from previous analysis
        if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
            match version {
                DYNAMIC_VERSION_V1 => {
                    // v1 format: max_dynamic_types, total_types, then type names as strings
                    writer.put_var_uint(DEFAULT_MAX_DYNAMIC_TYPES)?;
                    writer.put_var_uint(dynamic_state.total_types)?;
                    
                    // Write type names as strings (not DataType format)
                    for type_name in &dynamic_state.type_names {
                        writer.put_string(type_name)?;
                    }
                    
                    // Write variant serialization version (always 0)
                    writer.put_u64_le(0);
                    
                    // Clone to avoid borrowing issues
                    let type_names = dynamic_state.type_names.clone();
                    let type_map = dynamic_state.type_map.clone();
                    
                    // Write nested type prefixes
                    for type_name in &type_names {
                        let (_, typ) = &type_map[type_name];
                        typ.serialize_prefix(writer, state);
                    }
                }
                DYNAMIC_VERSION_V2 => {
                    // v2 format: total_types, then type names as strings
                    writer.put_var_uint(dynamic_state.total_types)?;
                    
                    // Write type names as strings (not DataType format)
                    for type_name in &dynamic_state.type_names {
                        writer.put_string(type_name)?;
                    }
                    
                    // Write variant serialization version (always 0)
                    writer.put_u64_le(0);
                    
                    // Clone to avoid borrowing issues
                    let type_names = dynamic_state.type_names.clone();
                    let type_map = dynamic_state.type_map.clone();
                    
                    // Write nested type prefixes
                    for type_name in &type_names {
                        let (_, typ) = &type_map[type_name];
                        typ.serialize_prefix(writer, state);
                    }
                }
                DYNAMIC_VERSION => {
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
                }
                _ => {
                    return Err(crate::Error::SerializeError(
                        format!("Unsupported Dynamic version: {version}")
                    ));
                }
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

        let version = Self::get_version(state);
        match version {
            DYNAMIC_VERSION_V1 | DYNAMIC_VERSION_V2 => {
                // v1 and v2 use the same variant data format with 8-bit discriminators
                Self::write_variant_data_v2_sync(&type_names, &type_map, values, writer, state)
            }
            DYNAMIC_VERSION => {
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
            _ => Err(crate::Error::SerializeError(
                format!("Unsupported Dynamic version: {version}")
            ))
        }
    }
}

#[cfg(test)]
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
        DynamicSerializer::write_prefix(&Type::Dynamic, &mut buffer, &mut state).await.unwrap();

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

    #[test]
    fn test_version_detection() {
        // Test v1 detection (< 24.11)
        let mut state = SerializerState::default();
        state.server_version = Some((24, 8, 1));
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION_V1);

        // Test v2 detection (24.11 - 25.5)
        state.server_version = Some((24, 11, 0));
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION_V2);

        state.server_version = Some((25, 1, 0));
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION_V2);

        state.server_version = Some((25, 5, 0));
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION_V2);

        // Test v3 detection (>= 25.6)
        state.server_version = Some((25, 6, 0));
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION);

        // Test default (no version)
        state.server_version = None;
        assert_eq!(DynamicSerializer::get_version(&state), DYNAMIC_VERSION);
    }

    #[test]
    fn test_dynamic_v2_prefix() {
        // Test v2 Dynamic prefix serialization
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        state.server_version = Some((25, 1, 0)); // Force v2

        // Create test data
        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(std::f64::consts::PI),
        ];

        // Analyze values
        let type_specific_state = DynamicSerializer::analyze_values(&values);
        state.type_specific = type_specific_state;

        // Write prefix
        DynamicSerializer::write_prefix_sync(&Type::Dynamic, &mut buffer, &mut state).unwrap();

        // Verify format
        let mut reader = &buffer[..];
        
        // Read version
        let version = reader.get_u64_le();
        assert_eq!(version, DYNAMIC_VERSION_V2);

        // Read total_types (no max_dynamic_types in v2)
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 3);

        // Read type names as strings
        let expected_types = vec!["Float64", "Int32", "String"];
        for expected_type in &expected_types {
            let bytes = reader.try_get_string().unwrap();
            let actual_type = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual_type, expected_type);
        }
        
        // Read variant version
        let variant_version = reader.get_u64_le();
        assert_eq!(variant_version, 0);
    }

    #[test]
    fn test_dynamic_v1_prefix() {
        // Test v1 Dynamic prefix serialization
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        state.server_version = Some((24, 8, 0)); // Force v1

        // Create test data
        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
        ];

        // Analyze values
        let type_specific_state = DynamicSerializer::analyze_values(&values);
        state.type_specific = type_specific_state;

        // Write prefix
        DynamicSerializer::write_prefix_sync(&Type::Dynamic, &mut buffer, &mut state).unwrap();

        // Verify format
        let mut reader = &buffer[..];
        
        // Read version
        let version = reader.get_u64_le();
        assert_eq!(version, DYNAMIC_VERSION_V1);

        // Read max_dynamic_types (v1 only)
        let max_dynamic_types = reader.try_get_var_uint().unwrap();
        assert_eq!(max_dynamic_types, DEFAULT_MAX_DYNAMIC_TYPES);

        // Read total_types
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 2);

        // Read type names as strings
        let expected_types = vec!["Int32", "String"];
        for expected_type in &expected_types {
            let bytes = reader.try_get_string().unwrap();
            let actual_type = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(&actual_type, expected_type);
        }
        
        // Read variant version
        let variant_version = reader.get_u64_le();
        assert_eq!(variant_version, 0);
    }

    #[test]
    fn test_discriminators_v2() {
        // Test v1/v2 discriminator building (8-bit, NULL=255)
        let values = vec![
            Value::Int32(42),
            Value::Null,
            Value::String(b"test".to_vec()),
            Value::Int32(99),
            Value::Null,
        ];

        let (_, type_map, _) = DynamicSerializer::build_type_registry(&values);
        let discriminators = DynamicSerializer::build_discriminators_v2(&values, &type_map);

        assert_eq!(discriminators.len(), 5);
        assert_eq!(discriminators[0], 0); // Int32 (first alphabetically)
        assert_eq!(discriminators[1], 255); // NULL
        assert_eq!(discriminators[2], 1); // String (second alphabetically)
        assert_eq!(discriminators[3], 0); // Int32
        assert_eq!(discriminators[4], 255); // NULL
    }

    #[test]
    fn test_dynamic_v2_data_writing() {
        // Test v2 data serialization
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        state.server_version = Some((25, 1, 0)); // Force v2

        let values = vec![
            Value::Int32(42),
            Value::Null,
            Value::String(b"hello".to_vec()),
            Value::Int32(99),
        ];

        // Analyze values
        let type_specific_state = DynamicSerializer::analyze_values(&values);
        state.type_specific = type_specific_state;

        // Write data
        DynamicSerializer::write_sync(&Type::Dynamic, &values, &mut buffer, &mut state).unwrap();

        // Verify discriminators are 8-bit
        let mut reader = &buffer[..];
        
        // Read variant discriminator mode (0 = BASIC)
        assert_eq!(reader.try_get_var_uint().unwrap(), 0);
        
        // Read discriminators (8-bit in v2)
        assert_eq!(reader.get_u8(), 0); // Int32
        assert_eq!(reader.get_u8(), 255); // NULL
        assert_eq!(reader.get_u8(), 1); // String
        assert_eq!(reader.get_u8(), 0); // Int32

        // The rest would be column data, which is complex to verify manually
        // but we've tested that the discriminators are written correctly
    }
    
    #[test]
    fn test_dynamic_v2_full_serialization() {
        // Test full v2 serialization including prefix
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        state.server_version = Some((25, 1, 0)); // Force v2

        let values = vec![
            Value::Int32(42),
            Value::String(b"hello".to_vec()),
            Value::Float64(3.14),
        ];

        // Analyze values
        let type_specific_state = DynamicSerializer::analyze_values(&values);
        state.type_specific = type_specific_state;

        // Write prefix
        DynamicSerializer::write_prefix_sync(&Type::Dynamic, &mut buffer, &mut state).unwrap();
        let prefix_len = buffer.len();
        
        // Write data
        DynamicSerializer::write_sync(&Type::Dynamic, &values, &mut buffer, &mut state).unwrap();
        
        println!("Dynamic v2 serialization:");
        println!("  Prefix ({} bytes): {:?}", prefix_len, &buffer[..prefix_len]);
        println!("  Data ({} bytes): {:?}", buffer.len() - prefix_len, &buffer[prefix_len..]);
        
        // Basic validation
        let mut reader = &buffer[..];
        
        // Read version
        let version = reader.get_u64_le();
        assert_eq!(version, 2);
        
        // Read total_types
        let total_types = reader.try_get_var_uint().unwrap();
        assert_eq!(total_types, 3);
    }
}
