use std::collections::HashMap;

use tokio::io::AsyncWriteExt;

use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::native::types::serialize::SerializerState;
use crate::native::types::{Type, Value};
use crate::Result;

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
                    grouped_values
                        .entry(*disc)
                        .or_default()
                        .push((**inner).clone());
                }
                _ => {
                    return Err(crate::Error::SerializeError(
                        "Expected Variant value".to_string()
                    ));
                }
            }
        }
        
        Ok((discriminators, grouped_values))
    }
    
    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Write version prefix (8 bytes of 0)
        writer.write_u64_le(0).await?;
        Ok(())
    }
    
    pub(crate) fn write_sync_prefix<W: ClickHouseBytesWrite>(
        _type_: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Write version prefix (8 bytes of 0)
        writer.put_u64_le(0);
        Ok(())
    }
    
    pub(crate) async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map = crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types)?;
        
        // Extract discriminators and group values
        let (discriminators, grouped_values) = Self::extract_discriminators_and_values(&values)?;
        
        // Write discriminators
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
        let discriminator_map = crate::native::types::deserialize::variant::DiscriminatorMap::new(variant_types)?;
        
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
    use super::*;
    use std::io::Cursor;
    use crate::native::types::deserialize::variant::VariantDeserializer;
    use crate::formats::DeserializerState;
    
    #[test]
    fn test_variant_serialization_simple() {
        // Test Variant(String, UInt64) with values ["hello", 42, "world"]
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);
        
        let values = vec![
            Value::Variant(0, Box::new(Value::String(b"hello".to_vec()))), // String
            Value::Variant(1, Box::new(Value::UInt64(42))),               // UInt64
            Value::Variant(0, Box::new(Value::String(b"world".to_vec()))), // String
        ];
        
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        
        // Write prefix  
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();
        
        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state).unwrap();
        
        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();
        
        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();
        
        // Read values
        let deserialized = VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state).unwrap();
        
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
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state).unwrap();
        
        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();
        
        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();
        
        // Read values
        let deserialized = VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state).unwrap();
        
        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized, values);
    }
    
    #[test]
    fn test_variant_serialization_complex_types() {
        // Test Variant(Array(String), Date)
        let variant_type = Type::Variant(vec![
            Type::Array(Box::new(Type::String)),
            Type::Date,
        ]);
        
        let values = vec![
            Value::Variant(0, Box::new(Value::Array(vec![
                Value::String(b"a".to_vec()),
                Value::String(b"b".to_vec()),
            ]))), // Array(String)
            Value::Variant(1, Box::new(Value::Date(crate::native::values::Date(19723)))), // Date
            Value::Variant(0, Box::new(Value::Array(vec![
                Value::String(b"c".to_vec()),
            ]))), // Array(String) with single element
        ];
        
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        
        // Write prefix  
        VariantSerializer::write_sync_prefix(&variant_type, &mut buffer, &mut state).unwrap();
        
        // Write values
        VariantSerializer::write_sync(&variant_type, values.clone(), &mut buffer, &mut state).unwrap();
        
        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();
        
        // Read prefix
        use bytes::Buf;
        let _ = reader.get_u64_le();
        
        // Read values
        let deserialized = VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut deser_state).unwrap();
        
        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized, values);
    }
    
    #[tokio::test]
    async fn test_variant_async_serialization() {
        // Test async version
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);
        
        let values = vec![
            Value::Variant(1, Box::new(Value::UInt64(999))),              // UInt64
            Value::Variant(0, Box::new(Value::String(b"async".to_vec()))), // String
        ];
        
        let mut buffer = Vec::new();
        let mut state = SerializerState::default();
        
        // Write prefix
        VariantSerializer::write_prefix(&variant_type, &mut buffer, &mut state).await.unwrap();
        
        // Write values
        VariantSerializer::write(&variant_type, values.clone(), &mut buffer, &mut state).await.unwrap();
        
        // Now deserialize and verify
        let mut reader = Cursor::new(buffer);
        let mut deser_state = DeserializerState::default();
        
        // Read prefix
        use tokio::io::AsyncReadExt;
        let _ = reader.read_u64_le().await.unwrap();
        
        // Read values
        let deserialized = VariantDeserializer::read_async(&variant_type, &mut reader, 2, &mut deser_state).await.unwrap();
        
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
        
        let (discriminators, grouped) = VariantSerializer::extract_discriminators_and_values(&values).unwrap();
        
        assert_eq!(discriminators, vec![0, 1, 0, 0xFF]);
        assert_eq!(grouped.len(), 3); // 0, 1, and 0xFF
        assert_eq!(grouped[&0].len(), 2); // Two strings
        assert_eq!(grouped[&1].len(), 1); // One UInt64
        assert_eq!(grouped[&0xFF].len(), 1); // One NULL
    }
}