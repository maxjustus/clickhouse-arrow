use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::formats::{DeserializerState, DynamicState, TypeSpecificState};
use crate::io::ClickHouseRead;
use crate::native::types::deserialize::{ClickHouseNativeDeserializer, read_discriminator};
use crate::native::types::{Type, Value};

// Dynamic serialization versions
const DYNAMIC_VERSION_V1: u64 = 0;
const DYNAMIC_VERSION_V2: u64 = 2;
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

        match version {
            DYNAMIC_VERSION_FLATTENED => {
                Self::read_data_flattened(reader, rows, state, total_types, &types).await
            }
            DYNAMIC_VERSION_V1 | DYNAMIC_VERSION_V2 => {
                Self::read_data_v1_v2(reader, rows, state, &types).await
            }
            _ => Err(crate::Error::DeserializeError(format!(
                "Unsupported Dynamic version: {version}"
            ))),
        }
    }

    /// Read data for FLATTENED (v3) format
    async fn read_data_flattened<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
        total_types: u64,
        types: &[(String, Type)],
    ) -> Result<Vec<Value>> {
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

    /// Read data for V1/V2 format (uses Variant serialization internally)
    /// Note: We can't use VariantDeserializer directly because it sorts types by name,
    /// but ClickHouse assigns discriminators in wire order.
    async fn read_data_v1_v2<R: ClickHouseRead>(
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
        types: &[(String, Type)],
    ) -> Result<Vec<Value>> {
        const NULL_DISCRIMINATOR: u8 = 0xFF;

        // Read discriminators (u8 per row, NOT sorted - in wire order)
        let mut discriminators = vec![0u8; rows];
        let _ = reader.read_exact(&mut discriminators).await?;

        // Build offsets and count rows per discriminator (same as Variant)
        let mut offsets = vec![0; rows];
        let mut row_count_by_type: HashMap<u8, usize> = HashMap::new();
        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != NULL_DISCRIMINATOR {
                let count = row_count_by_type.entry(disc).or_default();
                offsets[i] = *count;
                *count += 1;
            }
        }

        // Read column data for each type that has rows (in wire order, not sorted!)
        let mut columns: HashMap<u8, Vec<Value>> = HashMap::new();
        for (idx, (_, typ)) in types.iter().enumerate() {
            let discriminator = u8::try_from(idx).expect("Too many variant types");
            if let Some(&count) = row_count_by_type.get(&discriminator)
                && count > 0
            {
                let column_values = typ.deserialize_column(reader, count, state).await?;
                let old = columns.insert(discriminator, column_values);
                debug_assert!(old.is_none(), "Duplicate discriminator column");
            }
        }

        // Reconstruct values in original row order
        discriminators
            .iter()
            .zip(&offsets)
            .map(|(&disc, &offset)| {
                if disc == NULL_DISCRIMINATOR {
                    Ok(Value::Variant(disc, Box::new(Value::Null)))
                } else {
                    columns
                        .get(&disc)
                        .and_then(|col| col.get(offset))
                        .map(|val| Value::Variant(disc, Box::new(val.clone())))
                        .ok_or_else(|| {
                            crate::Error::DeserializeError(format!(
                                "Invalid offset {offset} for discriminator {disc}"
                            ))
                        })
                }
            })
            .collect()
    }

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        _type: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let version = reader.read_u64_le().await?;

        match version {
            DYNAMIC_VERSION_FLATTENED => Self::read_prefix_flattened(reader, state).await,
            DYNAMIC_VERSION_V1 | DYNAMIC_VERSION_V2 => {
                Self::read_prefix_v1_v2(version, reader, state).await
            }
            _ => Err(crate::Error::DeserializeError(format!(
                "Unsupported Dynamic version: {version}"
            ))),
        }
    }

    /// Read prefix for FLATTENED (v3) format
    async fn read_prefix_flattened<R: ClickHouseRead>(
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
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

    /// Read prefix for V1/V2 format (uses Variant internally)
    async fn read_prefix_v1_v2<R: ClickHouseRead>(
        version: u64,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // V1 has an extra max_dynamic_types parameter that we skip
        if version == DYNAMIC_VERSION_V1 {
            let _max_dynamic_types = reader.read_var_uint().await?;
        }

        // Read number of dynamic types
        let num_dynamic_types = reader.read_var_uint().await?;

        // Read type names
        let mut types = Vec::with_capacity(num_dynamic_types.try_into().unwrap_or(usize::MAX));
        for _ in 0..num_dynamic_types {
            types.push(Self::parse_type_entry(reader.read_string().await?)?);
        }

        // Add shared variant type (String) - this is always present in V1/V2
        // ColumnDynamic::getSharedVariantDataType() returns String
        types.push(("String".to_string(), Type::String));

        // V1/V2 Dynamic uses Variant serialization internally.
        // The Variant prefix includes a version u64, then nested prefixes.
        let _variant_version = reader.read_u64_le().await?;

        // Read prefixes for each nested type (including SharedVariant String)
        for (_, typ) in &types {
            typ.deserialize_prefix_async(reader, state).await?;
        }

        // Store metadata - we use the same structure but with version indicating V1/V2
        let total_types = types.len() as u64;
        let mut type_names = Vec::with_capacity(types.len());
        let mut type_map = HashMap::new();
        for (idx, (name, typ)) in types.iter().enumerate() {
            type_names.push(name.clone());
            drop(type_map.insert(name.clone(), (idx, typ.clone())));
        }

        state.type_specific = TypeSpecificState::Dynamic(DynamicState {
            version: Some(version),
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

    // Removed sync read_prefix; async-only path is supported.
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
