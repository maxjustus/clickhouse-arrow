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

    /// Write column data for each discriminator in ascending order
    async fn write_columns<W: ClickHouseWrite>(
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

    /// Write column data sync version
    fn write_columns_sync<W: ClickHouseBytesWrite>(
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

    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        writer.write_u64_le(VERSION).await?;

        // Write prefixes for nested types
        for inner_type in type_.unwrap_variant()? {
            inner_type.serialize_prefix_async(writer, state).await?;
        }
        Ok(())
    }

    pub(crate) fn write_sync_prefix<W: ClickHouseBytesWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
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

    /// Helper to serialize and deserialize variant values
    fn round_trip_test(variant_type: &Type, values: &[Value]) {
        use bytes::Buf;

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Serialize
        VariantSerializer::write_sync_prefix(variant_type, &mut buffer, &mut state).unwrap();
        VariantSerializer::write_sync(variant_type, values, &mut buffer, &mut state).unwrap();

        // Deserialize
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();
        let _ = reader.get_u64_le(); // Skip version

        let deserialized = VariantDeserializer::read_sync(
            variant_type,
            &mut reader,
            values.len(),
            &mut deser_state,
        )
        .unwrap();

        assert_eq!(deserialized, values);
    }

    /// Macro to create variant values
    macro_rules! variant {
        ($disc:expr, $value:expr) => {
            Value::Variant($disc, Box::new($value))
        };
    }

    /// Comprehensive macro to test Variant serialization functionality
    /// Consolidates 10 individual test functions into organized test groupings
    macro_rules! test_variant_serialization {
        () => {
            #[test]
            fn test_variant_basic_and_null_patterns() {
                // Test simple variant serialization
                let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);
                let values = vec![
                    variant!(0, Value::String(b"hello".to_vec())),
                    variant!(1, Value::UInt64(42)),
                    variant!(0, Value::String(b"world".to_vec())),
                ];
                round_trip_test(&variant_type, &values);

                // Test with nulls
                let values_with_nulls = vec![
                    variant!(0, Value::String(b"test".to_vec())),
                    variant!(0xFF, Value::Null),
                    variant!(1, Value::UInt64(123)),
                ];
                round_trip_test(&variant_type, &values_with_nulls);

                // Test empty variants
                round_trip_test(&variant_type, &[]);
            }

            #[test]
            fn test_variant_complex_and_nested_types() {
                // Test complex types (Arrays, Dates)
                let variant_type = Type::Variant(vec![Type::Array(Box::new(Type::String)), Type::Date]);
                let values = vec![
                    variant!(0, Value::Array(vec![Value::String(b"a".to_vec()), Value::String(b"b".to_vec())])),
                    variant!(1, Value::Date(Date(19723))),
                    variant!(0, Value::Array(vec![Value::String(b"c".to_vec())])),
                ];
                round_trip_test(&variant_type, &values);

                // Test nested types (Nullable, Tuple)
                let nested_type = Type::Variant(vec![
                    Type::String,
                    Type::Array(Box::new(Type::Nullable(Box::new(Type::UInt64)))),
                    Type::Tuple(vec![Type::String, Type::UInt64]),
                ]);
                let nested_values = vec![
                    variant!(1, Value::String(b"test_str".to_vec())),
                    variant!(0, Value::Array(vec![Value::UInt64(100), Value::Null, Value::UInt64(200)])),
                    variant!(2, Value::Tuple(vec![Value::String(b"tuple_str".to_vec()), Value::UInt64(42)])),
                ];
                round_trip_test(&nested_type, &nested_values);
            }

            #[test]
            fn test_variant_homogeneous_and_sparse_patterns() {
                use bytes::Buf;

                // Test homogeneous pattern (all same discriminator)
                let homo_type = Type::Variant(vec![Type::String, Type::UInt64, Type::Float64]);
                let homo_values: Vec<_> = (100..=500).step_by(100).map(|v| variant!(2, Value::UInt64(v))).collect();
                let mut buffer = Vec::new();
                let mut state = SerializerState::default();
                VariantSerializer::write_sync_prefix(&homo_type, &mut buffer, &mut state).unwrap();
                VariantSerializer::write_sync(&homo_type, &homo_values, &mut buffer, &mut state).unwrap();
                // Verify discriminators
                let mut reader = Cursor::new(&buffer);
                assert_eq!(reader.get_u64_le(), 0); // Version
                for _ in 0..5 {
                    assert_eq!(reader.get_u8(), 2); // All discriminators should be 2
                }

                // Test sparse pattern (mixed discriminators)
                let sparse_type = Type::Variant(vec![
                    Type::String,
                    Type::UInt64,
                    Type::Float64,
                    Type::Array(Box::new(Type::Int32)),
                    Type::Date,
                ]);
                let sparse_values = vec![
                    variant!(3, Value::String(b"test".to_vec())),
                    variant!(4, Value::UInt64(42)),
                    variant!(2, Value::Float64(std::f64::consts::PI)),
                    variant!(1, Value::Date(Date(19723))),
                    variant!(0, Value::Array(vec![Value::Int32(1), Value::Int32(2)])),
                    variant!(3, Value::String(b"another".to_vec())),
                    variant!(0xFF, Value::Null),
                    variant!(4, Value::UInt64(999)),
                ];
                round_trip_test(&sparse_type, &sparse_values);
            }

            #[test]
            fn test_variant_all_nulls_and_wire_format() {
                use bytes::Buf;

                // Test all nulls pattern
                let variant_type = Type::Variant(vec![Type::String, Type::UInt64, Type::Date]);
                let values = vec![variant!(0xFF, Value::Null); 4];
                let mut buffer = Vec::new();
                let mut state = SerializerState::default();
                VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();
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
            fn test_variant_extract_discriminators_and_grouping() {
                // Test discriminator extraction and value grouping
                let values = vec![
                    variant!(0, Value::String(b"a".to_vec())),
                    variant!(1, Value::UInt64(42)),
                    variant!(0, Value::String(b"b".to_vec())),
                    variant!(0xFF, Value::Null),
                ];
                let (discriminators, grouped) = VariantSerializer::extract_discriminators_and_values(&values).unwrap();
                assert_eq!(discriminators, vec![0, 1, 0, 0xFF]);
                assert_eq!(grouped.len(), 3);
                assert_eq!(grouped[&0].len(), 2);
                assert_eq!(grouped[&1].len(), 1);
                assert_eq!(grouped[&0xFF].len(), 1);

                // Test grouping with various discriminator patterns
                let mixed_values = vec![
                    variant!(2, Value::Float64(1.5)),
                    variant!(0, Value::String(b"first".to_vec())),
                    variant!(2, Value::Float64(2.5)),
                    variant!(1, Value::UInt64(100)),
                    variant!(0, Value::String(b"second".to_vec())),
                ];
                let (mixed_disc, mixed_grouped) = VariantSerializer::extract_discriminators_and_values(&mixed_values).unwrap();
                assert_eq!(mixed_disc, vec![2, 0, 2, 1, 0]);
                assert_eq!(mixed_grouped.len(), 3);
                assert_eq!(mixed_grouped[&0].len(), 2); // 2 strings
                assert_eq!(mixed_grouped[&1].len(), 1); // 1 uint64
                assert_eq!(mixed_grouped[&2].len(), 2); // 2 floats
            }

            #[tokio::test]
            async fn test_variant_async_serialization_comprehensive() {
                use tokio::io::AsyncReadExt;

                // Test basic async serialization
                let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);
                let values = vec![variant!(1, Value::UInt64(999)), variant!(0, Value::String(b"async".to_vec()))];
                let mut buffer = Vec::new();
                let mut state = SerializerState::default();
                VariantSerializer::write_prefix(&variant_type, &mut buffer, &mut state).await.unwrap();
                VariantSerializer::write(&variant_type, values.clone(), &mut buffer, &mut state).await.unwrap();
                
                // Verify async deserialization
                let mut reader = Cursor::new(buffer);
                let mut deser_state = DeserializerState::default();
                let _ = reader.read_u64_le().await.unwrap();
                let deserialized = VariantDeserializer::read_async(&variant_type, &mut reader, 2, &mut deser_state).await.unwrap();
                assert_eq!(deserialized, values);

                // Test async with complex types and nulls
                let complex_type = Type::Variant(vec![Type::Array(Box::new(Type::Int32)), Type::String, Type::Date]);
                let complex_values = vec![
                    variant!(0, Value::Array(vec![Value::Int32(10), Value::Int32(20)])),
                    variant!(0xFF, Value::Null),
                    variant!(1, Value::String(b"complex".to_vec())),
                    variant!(2, Value::Date(Date(20000))),
                ];
                let mut complex_buffer = Vec::new();
                let mut complex_state = SerializerState::default();
                VariantSerializer::write_prefix(&complex_type, &mut complex_buffer, &mut complex_state).await.unwrap();
                VariantSerializer::write(&complex_type, complex_values.clone(), &mut complex_buffer, &mut complex_state).await.unwrap();
                
                // Verify complex async round-trip
                let mut complex_reader = Cursor::new(complex_buffer);
                let mut complex_deser_state = DeserializerState::default();
                let _ = complex_reader.read_u64_le().await.unwrap();
                let complex_deserialized = VariantDeserializer::read_async(&complex_type, &mut complex_reader, 4, &mut complex_deser_state).await.unwrap();
                assert_eq!(complex_deserialized, complex_values);
            }

            // This single test module replaces 10 individual test functions (220+ lines)
            // while maintaining comprehensive test coverage of Variant serialization
        };
    }

    test_variant_serialization!();
}
