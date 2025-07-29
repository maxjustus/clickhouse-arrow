use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::formats::DeserializerState;
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::types::deserialize::ClickHouseNativeDeserializer;
use crate::native::types::{Type, Value};

/// State for storing Dynamic type metadata between prefix and data reading phases
#[derive(Debug, Default)]
pub(crate) struct DynamicState {
    version:     Option<u64>,
    total_types: Option<u64>,
    types:       Option<Vec<(String, Type)>>,
}

thread_local! {
    static DYNAMIC_STATE: std::cell::RefCell<DynamicState> = std::cell::RefCell::new(DynamicState::default());
}

/// Handles deserialization of Dynamic types
/// Dynamic is internally represented as a Variant with different serialization versions
pub(crate) struct DynamicDeserializer;

impl DynamicDeserializer {
    /// Determine discriminator size based on total types count
    /// (Kept for potential future use when we add v2 support)
    #[allow(dead_code)]
    fn discriminator_size(total_types: u64) -> usize {
        match total_types {
            0..=255 => 1,               // u8
            256..=65535 => 2,           // u16
            65536..=4_294_967_295 => 4, // u32
            _ => 8,                     // u64
        }
    }

    /// Read discriminator based on the total types count
    async fn read_discriminator<R: ClickHouseRead>(
        reader: &mut R,
        total_types: u64,
    ) -> Result<u64> {
        Ok(match total_types {
            0..=255 => u64::from(reader.read_u8().await?),
            256..=65535 => u64::from(reader.read_u16_le().await?),
            65536..=4_294_967_295 => u64::from(reader.read_u32_le().await?),
            _ => reader.read_u64_le().await?,
        })
    }

    /// Read discriminator sync version
    fn read_discriminator_sync<R: ClickHouseBytesRead>(
        reader: &mut R,
        total_types: u64,
    ) -> Result<u64> {
        Ok(match total_types {
            0..=255 => u64::from(reader.get_u8()),
            256..=65535 => u64::from(reader.get_u16_le()),
            65536..=4_294_967_295 => u64::from(reader.get_u32_le()),
            _ => reader.get_u64_le(),
        })
    }

    /// Read v3 header (CH 25.6+) - the only supported format
    async fn read_v3_header<R: ClickHouseRead>(
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<(u64, Vec<(String, Type)>)> {
        // Read total types count
        let total_types = reader.read_var_uint().await?;

        // Read type names and create types
        let mut types = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_types {
            let type_name_bytes = reader.read_string().await?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|e| {
                crate::Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}"))
            })?;
            let typ = type_name.parse::<Type>().map_err(|_| {
                crate::Error::DeserializeError(format!("Unknown type: {type_name}"))
            })?;
            types.push((type_name, typ));
        }

        // Read prefixes for nested types
        for (_, typ) in &types {
            typ.deserialize_prefix_async(reader, state).await?;
        }

        Ok((total_types, types))
    }

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        _type: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // Read serialization version
        let version = reader.read_u64_le().await?;

        // Store version and header info in state for use during data reading
        match version {
            3 => {
                // v3 flat format (CH 25.6+) - our primary supported format
                let (total_types, types) = Self::read_v3_header(reader, state).await?;
                DYNAMIC_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.version = Some(3);
                    state.total_types = Some(total_types);
                    state.types = Some(types);
                });
            }
            1 => {
                return Err(crate::Error::DeserializeError(
                    "Dynamic v1 serialization not supported. Use ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1' for v3 \
                     format."
                        .to_string(),
                ));
            }
            2 => {
                return Err(crate::Error::DeserializeError(
                    "Dynamic v2 serialization not supported. Use ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1' for v3 \
                     format."
                        .to_string(),
                ));
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "Unknown Dynamic serialization version: {version}. Expected version 3. Use \
                     ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1'."
                )));
            }
        }

        Ok(())
    }

    pub(crate) async fn read_async<R: ClickHouseRead>(
        _type: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Get stored metadata from prefix phase
        let (_version, total_types, types) = DYNAMIC_STATE.with(|state| {
            let state = state.borrow();
            let version = state.version.ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic version not set in state".to_string())
            })?;
            let total_types = state.total_types.ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic total types not set in state".to_string())
            })?;
            let types = state.types.clone().ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic types not set in state".to_string())
            })?;
            Ok::<_, crate::Error>((version, total_types, types))
        })?;

        // Read discriminators
        let mut discriminators = Vec::with_capacity(rows);
        for _ in 0..rows {
            let disc = Self::read_discriminator(reader, total_types).await?;
            discriminators.push(disc);
        }

        // Count rows per type
        let mut row_count_by_type: HashMap<u64, usize> = HashMap::new();
        let mut offsets = vec![0; rows];

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != total_types {
                // NULL discriminator is total_types in v2/v3
                let count = row_count_by_type.entry(disc).or_insert(0);
                offsets[i] = *count;
                *count += 1;
            }
        }

        // Read column data for each type
        let mut columns: HashMap<u64, Vec<Value>> = HashMap::new();

        for (idx, (_, typ)) in types.iter().enumerate() {
            let type_idx = idx as u64;
            if let Some(&count) = row_count_by_type.get(&type_idx)
                && count > 0
            {
                let column_values = typ.deserialize_column(reader, count, state).await?;
                drop(columns.insert(type_idx, column_values));
            }
        }

        // Reconstruct values in original order
        let mut values = Vec::with_capacity(rows);
        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == total_types {
                // NULL value
                values.push(Value::Null);
            } else if let Some(column) = columns.get(&disc) {
                let offset = offsets[i];
                if offset < column.len() {
                    // For Dynamic, we return the inner value directly (not wrapped in Variant)
                    values.push(column[offset].clone());
                } else {
                    return Err(crate::Error::DeserializeError(format!(
                        "Invalid offset {offset} for discriminator {disc}"
                    )));
                }
            } else {
                return Err(crate::Error::DeserializeError(format!(
                    "Unknown discriminator value: {disc}"
                )));
            }
        }

        Ok(values)
    }

    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        _type: &Type,
        reader: &mut R,
        _state: &mut DeserializerState,
    ) -> Result<()> {
        // Read serialization version
        let version = reader.get_u64_le();

        match version {
            3 => {
                // v3 flat format (CH 25.6+) - our primary supported format
                // Read total types
                let total_types = reader.try_get_var_uint()?;

                // Read type names
                let mut types = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
                for _ in 0..total_types {
                    let type_name_bytes = reader.try_get_string()?;
                    let type_name = String::from_utf8(type_name_bytes.to_vec()).map_err(|e| {
                        crate::Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}"))
                    })?;
                    let typ = type_name.parse::<Type>().map_err(|_| {
                        crate::Error::DeserializeError(format!("Unknown type: {type_name}"))
                    })?;
                    types.push((type_name, typ));
                }

                // Read prefixes for nested types
                for (_, typ) in &types {
                    typ.deserialize_prefix(reader)?;
                }

                DYNAMIC_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.version = Some(3);
                    state.total_types = Some(total_types);
                    state.types = Some(types);
                });
            }
            1 => {
                return Err(crate::Error::DeserializeError(
                    "Dynamic v1 serialization not supported. Use ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1' for v3 \
                     format."
                        .to_string(),
                ));
            }
            2 => {
                return Err(crate::Error::DeserializeError(
                    "Dynamic v2 serialization not supported. Use ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1' for v3 \
                     format."
                        .to_string(),
                ));
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "Unknown Dynamic serialization version: {version}. Expected version 3. Use \
                     ClickHouse 25.6+ and enable \
                     'output_format_native_use_flattened_dynamic_and_json_serialization=1'."
                )));
            }
        }

        Ok(())
    }

    pub(crate) fn read_sync<R: ClickHouseBytesRead>(
        _type: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Get stored metadata
        let (_version, total_types, types) = DYNAMIC_STATE.with(|state| {
            let state = state.borrow();
            let version = state.version.ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic version not set in state".to_string())
            })?;
            let total_types = state.total_types.ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic total types not set in state".to_string())
            })?;
            let types = state.types.clone().ok_or_else(|| {
                crate::Error::DeserializeError("Dynamic types not set in state".to_string())
            })?;
            Ok::<_, crate::Error>((version, total_types, types))
        })?;

        // Read discriminators
        let mut discriminators = Vec::with_capacity(rows);
        for _ in 0..rows {
            let disc = Self::read_discriminator_sync(reader, total_types)?;
            discriminators.push(disc);
        }

        // Count rows per type
        let mut row_count_by_type: HashMap<u64, usize> = HashMap::new();
        let mut offsets = vec![0; rows];

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != total_types {
                let count = row_count_by_type.entry(disc).or_insert(0);
                offsets[i] = *count;
                *count += 1;
            }
        }

        // Read column data
        let mut columns: HashMap<u64, Vec<Value>> = HashMap::new();

        for (idx, (_, typ)) in types.iter().enumerate() {
            let type_idx = idx as u64;
            if let Some(&count) = row_count_by_type.get(&type_idx)
                && count > 0
            {
                let column_values = typ.deserialize_column_sync(reader, count, state)?;
                drop(columns.insert(type_idx, column_values));
            }
        }

        // Reconstruct values
        let mut values = Vec::with_capacity(rows);
        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == total_types {
                values.push(Value::Null);
            } else if let Some(column) = columns.get(&disc) {
                let offset = offsets[i];
                if offset < column.len() {
                    values.push(column[offset].clone());
                } else {
                    return Err(crate::Error::DeserializeError(format!(
                        "Invalid offset {offset} for discriminator {disc}"
                    )));
                }
            } else {
                return Err(crate::Error::DeserializeError(format!(
                    "Unknown discriminator value: {disc}"
                )));
            }
        }

        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discriminator_size() {
        assert_eq!(DynamicDeserializer::discriminator_size(100), 1);
        assert_eq!(DynamicDeserializer::discriminator_size(255), 1);
        assert_eq!(DynamicDeserializer::discriminator_size(256), 2);
        assert_eq!(DynamicDeserializer::discriminator_size(65535), 2);
        assert_eq!(DynamicDeserializer::discriminator_size(65536), 4);
        assert_eq!(DynamicDeserializer::discriminator_size(4_294_967_295), 4);
        assert_eq!(DynamicDeserializer::discriminator_size(4_294_967_296), 8);
    }
}
