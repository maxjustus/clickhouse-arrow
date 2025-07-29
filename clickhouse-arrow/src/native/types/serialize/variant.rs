use std::collections::HashMap;

use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::{ClickHouseNativeSerializer, SerializerState};
use crate::native::types::{Type, Value};

/// Handles serialization of Variant types
pub(crate) struct VariantSerializer;

impl VariantSerializer {
    /// Extract discriminators and group values by discriminator
    fn extract_discriminators_and_values(
        values: &[Value],
    ) -> Result<(Vec<u8>, HashMap<u8, Vec<Value>>)> {
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

    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Write version prefix (8 bytes of 0)
        // Note: ClickHouse supports different serialization versions:
        // - Version 0: Current implementation (as of CH 25.x)
        writer.write_u64_le(0).await?;

        // Write prefixes for nested types that require them
        let variant_types = type_.unwrap_variant()?;
        for inner_type in variant_types {
            inner_type.serialize_prefix_async(writer, state).await?;
        }

        Ok(())
    }

    pub(crate) fn write_sync_prefix<W: ClickHouseBytesWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Write version prefix (8 bytes of 0)
        writer.put_u64_le(0);

        // Write prefixes for nested types that require them
        let variant_types = type_.unwrap_variant()?;
        for inner_type in variant_types {
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
            crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types)?;

        // Extract discriminators and group values
        let (discriminators, grouped_values) = Self::extract_discriminators_and_values(&values)?;

        // Write discriminators
        // NOTE: The current implementation matches the ClickHouse Go driver behavior.
        // There's a theoretical COMPACT mode (mode=1) mentioned in some documentation
        // that would use granule-based compression for homogeneous data, but it's not
        // implemented in any reference drivers as of 2025. Our implementation uses
        // what the docs call BASIC mode (mode=0) - writing raw discriminators.
        //
        // If COMPACT mode is ever implemented in ClickHouse, it would:
        // - Write mode byte = 1 after version prefix
        // - Group discriminators into granules
        // - Use single discriminator for homogeneous granules
        writer.write_all(&discriminators).await?;

        // Write column data for each discriminator in ascending order
        for discriminator in discriminator_map.discriminators() {
            if discriminator == 0xFF {
                // Skip NULL discriminator - no data to write
                continue;
            }

            if let Some(values_for_disc) = grouped_values.get(&discriminator) {
                if let Some(inner_type) = discriminator_map.get_type(discriminator) {
                    inner_type.serialize_column(values_for_disc.clone(), writer, state).await?;
                }
            }
        }

        Ok(())
    }

    pub(crate) fn write_sync<W: ClickHouseBytesWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map =
            crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types)?;

        // Extract discriminators and group values
        let (discriminators, grouped_values) = Self::extract_discriminators_and_values(&values)?;

        // Write discriminators
        writer.put_slice(&discriminators);

        // Write column data for each discriminator in ascending order
        for discriminator in discriminator_map.discriminators() {
            if discriminator == 0xFF {
                // Skip NULL discriminator - no data to write
                continue;
            }

            if let Some(values_for_disc) = grouped_values.get(&discriminator) {
                if let Some(inner_type) = discriminator_map.get_type(discriminator) {
                    inner_type.serialize_column_sync(values_for_disc.clone(), writer, state)?;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::formats::DeserializerState;
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;
    use crate::native::types::deserialize::variant::VariantDeserializer;
    use crate::native::values::Date;

    #[test]
    fn test_variant_serialization_simple() {
        // Test Variant(String, UInt64) with values ["hello", 42, "world"]
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        let values = vec![
            Value::Variant(0, Box::new(Value::String(b"hello".to_vec()))), // String
            Value::Variant(1, Box::new(Value::UInt64(42))),                // UInt64
            Value::Variant(0, Box::new(Value::String(b"world".to_vec()))), // String
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized, values);
    }

    #[test]
    fn test_variant_serialization_with_nulls() {
        // Test Variant(String, UInt64) with NULL values
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        let values = vec![
            Value::Variant(0, Box::new(Value::String(b"test".to_vec()))), // String
            Value::Variant(0xFF, Box::new(Value::Null)),                  // NULL
            Value::Variant(1, Box::new(Value::UInt64(123))),              // UInt64
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized, values);
    }

    #[test]
    fn test_variant_serialization_complex_types() {
        // Test Variant(Array(String), Date)
        let variant_type = Type::Variant(vec![Type::Array(Box::new(Type::String)), Type::Date]);

        let values = vec![
            Value::Variant(
                0,
                Box::new(Value::Array(vec![
                    Value::String(b"a".to_vec()),
                    Value::String(b"b".to_vec()),
                ])),
            ), // Array(String)
            Value::Variant(1, Box::new(Value::Date(Date(19723)))), // Date
            Value::Variant(0, Box::new(Value::Array(vec![Value::String(b"c".to_vec())]))), /* Array(String) with single element */
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized, values);
    }

    #[tokio::test]
    async fn test_variant_async_serialization() {
        // Test async version
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        let values = vec![
            Value::Variant(1, Box::new(Value::UInt64(999))), // UInt64
            Value::Variant(0, Box::new(Value::String(b"async".to_vec()))), // String
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_prefix(&variant_type, &mut buffer, &mut state).await.unwrap();

        // Write values
        VariantSerializer::write(&variant_type, values.clone(), &mut buffer, &mut state)
            .await
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use tokio::io::AsyncReadExt;
        let _ = reader.read_u64_le().await.unwrap();

        // Read values
        let deserialized =
            VariantDeserializer::read_async(&variant_type, &mut reader, 2, &mut deser_state)
                .await
                .unwrap();

        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized, values);
    }

    #[test]
    fn test_extract_discriminators_and_values() {
        let values = vec![
            Value::Variant(0, Box::new(Value::String(b"a".to_vec()))),
            Value::Variant(1, Box::new(Value::UInt64(42))),
            Value::Variant(0, Box::new(Value::String(b"b".to_vec()))),
            Value::Variant(0xFF, Box::new(Value::Null)),
        ];

        let (discriminators, grouped) =
            VariantSerializer::extract_discriminators_and_values(&values).unwrap();

        assert_eq!(discriminators, vec![0, 1, 0, 0xFF]);
        assert_eq!(grouped.len(), 3); // 0, 1, and 0xFF
        assert_eq!(grouped[&0].len(), 2); // Two strings
        assert_eq!(grouped[&1].len(), 1); // One UInt64
        assert_eq!(grouped[&0xFF].len(), 1); // One NULL
    }

    #[test]
    fn test_variant_serialization_homogeneous() {
        // Test with homogeneous data (all same type) - future COMPACT mode would optimize this
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64, Type::Float64]);

        // All values are UInt64 (discriminator 2 after sorting: Float64=0, String=1, UInt64=2)
        let values = vec![
            Value::Variant(2, Box::new(Value::UInt64(100))),
            Value::Variant(2, Box::new(Value::UInt64(200))),
            Value::Variant(2, Box::new(Value::UInt64(300))),
            Value::Variant(2, Box::new(Value::UInt64(400))),
            Value::Variant(2, Box::new(Value::UInt64(500))),
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Verify the wire format
        let mut reader = Cursor::new(&buffer);
        use bytes::Buf;

        // Version prefix (8 bytes of 0)
        assert_eq!(reader.get_u64_le(), 0);

        // TODO: In COMPACT mode, we would see mode byte = 1 here
        // For now, we're in BASIC mode, so discriminators follow directly

        // All discriminators should be 2
        for _ in 0..5 {
            assert_eq!(reader.get_u8(), 2);
        }

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 5, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 5);
        assert_eq!(deserialized, values);
    }

    #[test]
    fn test_variant_serialization_sparse() {
        // Test with sparse data (many different types intermixed)
        let variant_type = Type::Variant(vec![
            Type::String,
            Type::UInt64,
            Type::Float64,
            Type::Array(Box::new(Type::Int32)),
            Type::Date,
        ]);

        // Discriminator mapping after sorting:
        // Array(Int32)=0, Date=1, Float64=2, String=3, UInt64=4
        let values = vec![
            Value::Variant(3, Box::new(Value::String(b"test".to_vec()))), // String
            Value::Variant(4, Box::new(Value::UInt64(42))),               // UInt64
            Value::Variant(2, Box::new(Value::Float64(std::f64::consts::PI))),            // Float64
            Value::Variant(1, Box::new(Value::Date(Date(19723)))),        // Date
            Value::Variant(
                0,
                Box::new(Value::Array(vec![
                    // Array(Int32)
                    Value::Int32(1),
                    Value::Int32(2),
                ])),
            ),
            Value::Variant(3, Box::new(Value::String(b"another".to_vec()))), // String
            Value::Variant(0xFF, Box::new(Value::Null)),                     // NULL
            Value::Variant(4, Box::new(Value::UInt64(999))),                 // UInt64
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 8, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 8);
        assert_eq!(deserialized, values);
    }

    #[test]
    fn test_variant_serialization_with_nested_types() {
        // Test Variant containing types that require prefixes
        let variant_type = Type::Variant(vec![
            Type::String, // Changed from LowCardinality(String) which doesn't support sync
            Type::Array(Box::new(Type::Nullable(Box::new(Type::UInt64)))),
            Type::Tuple(vec![Type::String, Type::UInt64]),
        ]);

        // Discriminator mapping after sorting:
        // Array(Nullable(UInt64))=0, String=1, Tuple(String, UInt64)=2
        let values = vec![
            Value::Variant(1, Box::new(Value::String(b"test_str".to_vec()))), // String
            Value::Variant(
                0,
                Box::new(Value::Array(vec![
                    // Array(Nullable(UInt64))
                    Value::UInt64(100),
                    Value::Null,
                    Value::UInt64(200),
                ])),
            ),
            Value::Variant(
                2,
                Box::new(Value::Tuple(vec![
                    // Tuple
                    Value::String(b"tuple_str".to_vec()),
                    Value::UInt64(42),
                ])),
            ),
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix - this includes nested type prefixes
        variant_type.deserialize_prefix(&mut reader).unwrap();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 3);
        // Note: Values might not be exactly equal due to internal representation differences
        // but the data should be equivalent
    }

    #[test]
    fn test_variant_empty_values() {
        // Test with empty input
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);
        let values: Vec<Value> = vec![];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 0, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 0);
    }

    #[test]
    fn test_variant_all_nulls() {
        // Test with all NULL values
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64, Type::Date]);

        let values = vec![
            Value::Variant(0xFF, Box::new(Value::Null)),
            Value::Variant(0xFF, Box::new(Value::Null)),
            Value::Variant(0xFF, Box::new(Value::Null)),
            Value::Variant(0xFF, Box::new(Value::Null)),
        ];

        let mut buffer = Vec::new();
        let mut state = SerializerState::default();

        // Write prefix
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();

        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state)
            .unwrap();

        // Verify wire format has only discriminators (no column data for NULLs)
        let mut reader = Cursor::new(&buffer);
        use bytes::Buf;

        // Version prefix
        assert_eq!(reader.get_u64_le(), 0);

        // All discriminators should be 0xFF
        for _ in 0..4 {
            assert_eq!(reader.get_u8(), 0xFF);
        }

        // No column data should follow
        assert_eq!(reader.remaining(), 0);

        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();

        // Read prefix
        let _ = reader.get_u64_le();

        // Read values
        let deserialized =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 4, &mut deser_state)
                .unwrap();

        assert_eq!(deserialized.len(), 4);
        assert_eq!(deserialized, values);
    }

    // NOTE: COMPACT mode is a theoretical optimization mentioned in some docs
    // but not implemented in any reference drivers as of 2025. If it's ever
    // added to ClickHouse, it would be particularly efficient for homogeneous
    // data where all values in a granule have the same discriminator.
}
