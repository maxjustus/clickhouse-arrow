use std::collections::HashMap;

use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::{ClickHouseNativeSerializer, SerializerState};
use crate::native::types::{Type, Value};

type VariantGroupedData = (Vec<u8>, HashMap<u8, Vec<Value>>);

const VERSION: u64 = 0;
const NULL_DISCRIMINATOR: u8 = 0xFF;

/// Handles serialization of Variant types
pub(crate) struct VariantSerializer;

impl VariantSerializer {
    /// Extract discriminators and group values by discriminator
    fn extract_discriminators_and_values(values: &[Value]) -> Result<VariantGroupedData> {
        let mut discriminators = Vec::with_capacity(values.len());
        let mut grouped_values: HashMap<u8, Vec<Value>> = HashMap::new();

        for value in values {
            match value {
                Value::Variant(disc, inner) => {
                    discriminators.push(*disc);
                    grouped_values.entry(*disc).or_default().push((**inner).clone());
                }
                _ => {
                    return Err(crate::Error::SerializeError("Expected Variant value".to_string()));
                }
            }
        }

        Ok((discriminators, grouped_values))
    }

    /// Write column data for each discriminator (async version)
    async fn write_columns_internal_async<W: ClickHouseWrite>(
        discriminator_map: &crate::native::types::deserialize::variant::DiscriminatorMap,
        grouped_values: &HashMap<u8, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for &discriminator in discriminator_map.discriminators() {
            if discriminator == NULL_DISCRIMINATOR {
                continue; // Skip NULL discriminator - no data to write
            }

            if let Some(values_for_disc) = grouped_values.get(&discriminator)
                && let Some(inner_type) = discriminator_map.get_type(discriminator)
            {
                inner_type.serialize_column(values_for_disc.clone(), writer, state).await?;
            }
        }
        Ok(())
    }

    /// Write column data for each discriminator (sync version)
    fn write_columns_internal_sync<W: ClickHouseBytesWrite>(
        discriminator_map: &crate::native::types::deserialize::variant::DiscriminatorMap,
        grouped_values: &HashMap<u8, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        for &discriminator in discriminator_map.discriminators() {
            if discriminator == NULL_DISCRIMINATOR {
                continue; // Skip NULL discriminator - no data to write
            }

            if let Some(values_for_disc) = grouped_values.get(&discriminator)
                && let Some(inner_type) = discriminator_map.get_type(discriminator)
            {
                inner_type.serialize_column_sync(values_for_disc.clone(), writer, state)?;
            }
        }
        Ok(())
    }

    /// Write column data for each discriminator in ascending order
    async fn write_columns<W: ClickHouseWrite>(
        discriminator_map: &crate::native::types::deserialize::variant::DiscriminatorMap,
        grouped_values: &HashMap<u8, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        Self::write_columns_internal_async(discriminator_map, grouped_values, writer, state).await
    }

    /// Write column data sync version
    fn write_columns_sync<W: ClickHouseBytesWrite>(
        discriminator_map: &crate::native::types::deserialize::variant::DiscriminatorMap,
        grouped_values: &HashMap<u8, Vec<Value>>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        Self::write_columns_internal_sync(discriminator_map, grouped_values, writer, state)
    }

    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Write version. JSON FLATTENED relies on nested serializer prefixes for typed paths.
        // We call these with a fresh SerializerState (no type-specific state) to avoid
        // state leakage across paths.
        writer.write_u64_le(VERSION).await?;

        // Write prefixes for nested types
        for inner_type in type_.unwrap_variant()? {
            inner_type.serialize_prefix_async(writer, state).await?;
        }
        Ok(())
    }

    pub(crate) fn write_prefix_sync<W: ClickHouseBytesWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Write version. JSON FLATTENED relies on nested serializer prefixes for typed paths.
        // We call these with a fresh SerializerState (no type-specific state) to avoid
        // state leakage across paths.
        writer.put_u64_le(VERSION);

        // Write prefixes for nested types
        for inner_type in type_.unwrap_variant()? {
            inner_type.serialize_prefix(writer, state);
        }
        Ok(())
    }

    pub(crate) async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map =
            crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types);

        // Extract discriminators and group values
        let (discriminators, grouped_values) = Self::extract_discriminators_and_values(&values)?;

        // Write discriminators
        writer.write_all(&discriminators).await?;

        // Write column data
        Self::write_columns(&discriminator_map, &grouped_values, writer, state).await
    }

    pub(crate) fn write_sync<W: ClickHouseBytesWrite>(
        type_: &Type,
        values: &[Value],
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map =
            crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types);

        // Extract discriminators and group values
        let (discriminators, grouped_values) = Self::extract_discriminators_and_values(values)?;

        // Write discriminators
        writer.put_slice(&discriminators);

        // Write column data
        Self::write_columns_sync(&discriminator_map, &grouped_values, writer, state)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::formats::DeserializerState;
    use crate::native::types::deserialize::variant::VariantDeserializer;
    use crate::native::values::Date;

    /// Macro to test variant serialization roundtrip for both sync and async paths
    macro_rules! variant_roundtrip_test {
        ($name:ident, $variant_type:expr, $values:expr) => {
            #[tokio::test]
            async fn $name() {
                let variant_type = $variant_type;
                let values = $values;

                // Test sync path
                let mut sync_buffer = Vec::new();
                let mut sync_state = SerializerState::default();
                VariantSerializer::write_prefix_sync(
                    &variant_type,
                    &mut sync_buffer,
                    &mut sync_state,
                )
                .unwrap();
                VariantSerializer::write_sync(
                    &variant_type,
                    &values,
                    &mut sync_buffer,
                    &mut sync_state,
                )
                .unwrap();

                let mut sync_reader = Cursor::new(&sync_buffer);
                let mut sync_deser_state = DeserializerState::default();
                let _ = {
                    use bytes::Buf;
                    sync_reader.get_u64_le()
                }; // Skip version
                let sync_deserialized = VariantDeserializer::read_sync(
                    &variant_type,
                    &mut sync_reader,
                    values.len(),
                    &mut sync_deser_state,
                )
                .unwrap();

                // Test async path
                let mut async_buffer = Vec::new();
                let mut async_state = SerializerState::default();
                VariantSerializer::write_prefix(&variant_type, &mut async_buffer, &mut async_state)
                    .await
                    .unwrap();
                VariantSerializer::write(
                    &variant_type,
                    values.clone(),
                    &mut async_buffer,
                    &mut async_state,
                )
                .await
                .unwrap();

                let mut async_reader = Cursor::new(&async_buffer);
                let mut async_deser_state = DeserializerState::default();
                let _ = {
                    use tokio::io::AsyncReadExt;
                    async_reader.read_u64_le().await.unwrap()
                }; // Skip version
                let async_deserialized = VariantDeserializer::read_async(
                    &variant_type,
                    &mut async_reader,
                    values.len(),
                    &mut async_deser_state,
                )
                .await
                .unwrap();

                // Assert both paths produce the same results
                assert_eq!(sync_deserialized, values);
                assert_eq!(async_deserialized, values);
                assert_eq!(
                    sync_deserialized, async_deserialized,
                    "Sync and async results should match"
                );
            }
        };
    }

    /// Macro to create variant values
    macro_rules! variant {
        ($disc:expr, $value:expr) => {
            Value::Variant($disc, Box::new($value))
        };
    }

    // Use the macro to create sync/async test pairs
    variant_roundtrip_test!(
        test_variant_simple,
        Type::variant(vec![Type::String, Type::UInt64]),
        vec![
            variant!(0, Value::String(b"hello".to_vec())),
            variant!(1, Value::UInt64(42)),
            variant!(0, Value::String(b"world".to_vec())),
        ]
    );

    variant_roundtrip_test!(
        test_variant_with_nulls,
        Type::variant(vec![Type::String, Type::UInt64]),
        vec![
            variant!(0, Value::String(b"test".to_vec())),
            variant!(0xFF, Value::Null),
            variant!(1, Value::UInt64(123)),
        ]
    );

    variant_roundtrip_test!(
        test_variant_complex_types,
        Type::variant(vec![Type::Array(Box::new(Type::String)), Type::Date]),
        vec![
            variant!(
                0,
                Value::Array(vec![Value::String(b"a".to_vec()), Value::String(b"b".to_vec())])
            ),
            variant!(1, Value::Date(Date(19723))),
            variant!(0, Value::Array(vec![Value::String(b"c".to_vec())])),
        ]
    );

    #[test]
    fn test_variant_homogeneous() {
        use bytes::Buf;

        let variant_type = Type::variant(vec![Type::String, Type::UInt64, Type::Float64]);
        // All UInt64 (discriminator 2 after sorting)
        let values: Vec<_> =
            (100..=500).step_by(100).map(|v| variant!(2, Value::UInt64(v))).collect();

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        VariantSerializer::write_prefix_sync(&variant_type, &mut buffer, &mut state).unwrap();
        VariantSerializer::write_sync(&variant_type, &values, &mut buffer, &mut state).unwrap();

        // Verify discriminators
        let mut reader = Cursor::new(&buffer);
        assert_eq!(reader.get_u64_le(), 0); // Version
        for _ in 0..5 {
            assert_eq!(reader.get_u8(), 2); // All discriminators should be 2
        }
    }

    variant_roundtrip_test!(
        test_variant_sparse,
        Type::variant(vec![
            Type::String,
            Type::UInt64,
            Type::Float64,
            Type::Array(Box::new(Type::Int32)),
            Type::Date,
        ]),
        vec![
            variant!(3, Value::String(b"test".to_vec())),
            variant!(4, Value::UInt64(42)),
            variant!(2, Value::Float64(std::f64::consts::PI)),
            variant!(1, Value::Date(Date(19723))),
            variant!(0, Value::Array(vec![Value::Int32(1), Value::Int32(2)])),
            variant!(3, Value::String(b"another".to_vec())),
            variant!(0xFF, Value::Null),
            variant!(4, Value::UInt64(999)),
        ]
    );

    variant_roundtrip_test!(
        test_variant_with_nested_types,
        Type::variant(vec![
            Type::String,
            Type::Array(Box::new(Type::Nullable(Box::new(Type::UInt64)))),
            Type::Tuple(vec![Type::String, Type::UInt64]),
        ]),
        vec![
            variant!(1, Value::String(b"test_str".to_vec())),
            variant!(0, Value::Array(vec![Value::UInt64(100), Value::Null, Value::UInt64(200)])),
            variant!(
                2,
                Value::Tuple(vec![Value::String(b"tuple_str".to_vec()), Value::UInt64(42)])
            ),
        ]
    );

    variant_roundtrip_test!(
        test_variant_empty,
        Type::variant(vec![Type::String, Type::UInt64]),
        vec![]
    );

    #[test]
    fn test_variant_all_nulls() {
        use bytes::Buf;

        let variant_type = Type::variant(vec![Type::String, Type::UInt64, Type::Date]);
        let values = vec![variant!(0xFF, Value::Null); 4];
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        VariantSerializer::write_prefix_sync(&variant_type, &mut buffer, &mut state).unwrap();
        VariantSerializer::write_sync(&variant_type, &values, &mut buffer, &mut state).unwrap();

        // Verify wire format
        let mut reader = Cursor::new(&buffer);
        assert_eq!(reader.get_u64_le(), 0); // Version
        for _ in 0..4 {
            assert_eq!(reader.get_u8(), 0xFF); // All NULL discriminators
        }
        assert_eq!(reader.remaining(), 0); // No column data
    }

    #[test]
    fn test_extract_discriminators() {
        let values = vec![
            variant!(0, Value::String(b"a".to_vec())),
            variant!(1, Value::UInt64(42)),
            variant!(0, Value::String(b"b".to_vec())),
            variant!(0xFF, Value::Null),
        ];
        let (discriminators, grouped) =
            VariantSerializer::extract_discriminators_and_values(&values).unwrap();
        assert_eq!(discriminators, vec![0, 1, 0, 0xFF]);
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[&0].len(), 2);
        assert_eq!(grouped[&1].len(), 1);
        assert_eq!(grouped[&0xFF].len(), 1);
    }

    variant_roundtrip_test!(
        test_variant_async,
        Type::variant(vec![Type::String, Type::UInt64]),
        vec![variant!(1, Value::UInt64(999)), variant!(0, Value::String(b"async".to_vec()))]
    );
}
