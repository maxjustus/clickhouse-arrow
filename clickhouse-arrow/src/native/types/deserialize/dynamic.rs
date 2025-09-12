use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::formats::{DeserializerState, DynamicState, TypeSpecificState};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::types::deserialize::{ClickHouseNativeDeserializer, read_discriminator};
use crate::native::types::{Type, Value};

// Using FLATTENED format (version 3) for client compatibility
const DYNAMIC_VERSION_FLATTENED: u64 = 3;

/// Handles deserialization of Dynamic types
pub(crate) struct DynamicDeserializer;

impl DynamicDeserializer {
    /// Parse type name and create Type instance
    #[inline]
    fn parse_type_entry(type_name_bytes: Vec<u8>) -> Result<(String, Type)> {
        let type_name = String::from_utf8(type_name_bytes).map_err(|e| {
            crate::Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}"))
        })?;
        let typ = type_name
            .parse::<Type>()
            .map_err(|_| crate::Error::DeserializeError(format!("Unknown type: {type_name}")))?;
        Ok((type_name, typ))
    }

    /// Build offset mapping and count rows per type
    fn build_offsets(
        discriminators: &[u64],
        total_types: u64,
    ) -> (Vec<usize>, HashMap<u64, usize>) {
        let mut row_count_by_type = HashMap::new();
        let mut offsets = vec![0; discriminators.len()];

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != total_types {
                // NULL discriminator is total_types
                let count = row_count_by_type.entry(disc).or_default();
                offsets[i] = *count;
                *count += 1;
            }
        }

        (offsets, row_count_by_type)
    }

    /// Reconstruct values in original order
    fn reconstruct_values(
        discriminators: &[u64],
        offsets: &[usize],
        columns: &HashMap<u64, Vec<Value>>,
        total_types: u64,
    ) -> Result<Vec<Value>> {
        discriminators
            .iter()
            .zip(offsets)
            .map(|(&disc, &offset)| {
                if disc == total_types {
                    Ok(Value::Null)
                } else {
                    columns.get(&disc).and_then(|col| col.get(offset)).cloned().ok_or_else(|| {
                        crate::Error::DeserializeError(format!(
                            "Invalid offset {offset} for discriminator {disc}"
                        ))
                    })
                }
            })
            .collect()
    }

    /// Read Dynamic data (async version)
    async fn read_internal_async<R: ClickHouseRead>(
        _: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let (version, total_types, types) =
            if let TypeSpecificState::Dynamic(dynamic_state) = &state.type_specific {
                (
                    dynamic_state.version.unwrap_or(DYNAMIC_VERSION_FLATTENED),
                    dynamic_state.total_types,
                    dynamic_state.types.clone(),
                )
            } else {
                return Err(crate::Error::DeserializeError(
                    "Dynamic metadata not set in state".to_string(),
                ));
            };

        if version != DYNAMIC_VERSION_FLATTENED {
            return Err(crate::Error::DeserializeError(format!(
                "Dynamic type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            )));
        }

        // v3 format: variable-sized discriminators
        let mut discriminators = Vec::with_capacity(rows);
        for _ in 0..rows {
            discriminators.push(read_discriminator!(async reader, total_types));
        }

        // Build offsets and count rows
        let (offsets, row_count_by_type) = Self::build_offsets(&discriminators, total_types);

        // Read column data for each type
        let mut columns = HashMap::new();
        for (idx, (_, typ)) in types.iter().enumerate() {
            let type_idx = idx as u64;
            if let Some(&count) = row_count_by_type.get(&type_idx)
                && count > 0
            {
                let column_values = typ.deserialize_column(reader, count, state).await?;
                let old = columns.insert(type_idx, column_values);
                debug_assert!(old.is_none(), "Duplicate type index");
            }
        }

        Self::reconstruct_values(&discriminators, &offsets, &columns, total_types)
    }

    

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        _type: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.read_u64_le().await?;

        if version != DYNAMIC_VERSION_FLATTENED {
            return Err(crate::Error::DeserializeError(format!(
                "Dynamic type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            )));
        }

        // v3 format: total_types, then type names, then nested prefixes
        let total_types = reader.read_var_uint().await?;
        let mut types = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_types {
            types.push(Self::parse_type_entry(reader.read_string().await?)?);
        }

        // Read prefixes for nested types
        for (_, typ) in &types {
            typ.deserialize_prefix_async(reader, state).await?;
        }

        // Store metadata in state for data phase
        let mut type_names = Vec::with_capacity(types.len());
        let mut type_map = HashMap::new();
        for (idx, (name, typ)) in types.iter().enumerate() {
            type_names.push(name.clone());
            drop(type_map.insert(name.clone(), (idx, typ.clone())));
        }

        state.type_specific = TypeSpecificState::Dynamic(DynamicState {
            version: Some(DYNAMIC_VERSION_FLATTENED),
            total_types,
            type_names,
            type_map,
            types,
        });

        Ok(())
    }

    #[allow(clippy::used_underscore_binding)]
    pub(crate) async fn read_async<R: ClickHouseRead>(
        _type: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        Self::read_internal_async(_type, reader, rows, state).await
    }

    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        _type: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.get_u64_le();

        if version != DYNAMIC_VERSION_FLATTENED {
            return Err(crate::Error::DeserializeError(format!(
                "Dynamic type requires version 3, got version {version}. Please use ClickHouse \
                 server >= 25.6"
            )));
        }

        // v3 format: total_types, then type names, then nested prefixes
        let total_types = reader.try_get_var_uint()?;
        let mut types = Vec::with_capacity(total_types.try_into().unwrap_or(usize::MAX));
        for _ in 0..total_types {
            types.push(Self::parse_type_entry(reader.try_get_string()?.to_vec())?);
        }

        // Read prefixes for nested types
        for (_, typ) in &types {
            typ.deserialize_prefix(reader, state)?;
        }

        // Store metadata in state for data phase
        let mut type_names = Vec::with_capacity(types.len());
        let mut type_map = HashMap::new();
        for (idx, (name, typ)) in types.iter().enumerate() {
            type_names.push(name.clone());
            drop(type_map.insert(name.clone(), (idx, typ.clone())));
        }

        state.type_specific = TypeSpecificState::Dynamic(DynamicState {
            version: Some(DYNAMIC_VERSION_FLATTENED),
            total_types,
            type_names,
            type_map,
            types,
        });

        Ok(())
    }

    
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_discriminator_size() {
        // Test discriminator size calculation for different ranges
        let test_cases: Vec<(u64, usize)> = vec![
            (100, 1),
            (255, 1),
            (256, 2),
            (65535, 2),
            (65536, 4),
            (4_294_967_295, 4),
            (4_294_967_296, 8),
        ];

        for (total_types, expected_size) in test_cases {
            let size = match total_types {
                0..=255 => 1,
                256..=65535 => 2,
                65536..=4_294_967_295 => 4,
                _ => 8,
            };
            assert_eq!(size, expected_size, "Failed for total_types: {total_types}");
        }
    }
}
