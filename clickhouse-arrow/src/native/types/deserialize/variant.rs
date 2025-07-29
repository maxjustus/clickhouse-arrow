use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::types::deserialize::{ClickHouseNativeDeserializer, DeserializerState};
use crate::native::types::{Type, Value};

/// Represents a mapping from discriminator values to types and their string representations
#[derive(Debug, Clone)]
pub(crate) struct DiscriminatorMap {
    /// Maps discriminator byte to (type_string, type)
    types: HashMap<u8, (String, Type)>,
}

impl DiscriminatorMap {
    /// Create a new discriminator map from variant types
    pub(crate) fn new(variant_types: &[Type]) -> Result<Self> {
        // Convert types to their string representations and collect them
        let mut type_strings: Vec<(String, Type)> =
            variant_types.iter().map(|t| (t.to_string(), t.clone())).collect();

        // Sort by type string alphabetically to determine discriminator assignment
        type_strings.sort_by(|a, b| a.0.cmp(&b.0));

        // Build the discriminator map, starting from 0
        let mut types = HashMap::new();
        for (idx, (type_str, type_)) in type_strings.into_iter().enumerate() {
            let discriminator = idx as u8;
            drop(types.insert(discriminator, (type_str.clone(), type_)));
        }

        Ok(Self { types })
    }

    /// Get the type for a given discriminator
    pub(crate) fn get_type(&self, discriminator: u8) -> Option<&Type> {
        self.types.get(&discriminator).map(|(_, t)| t)
    }

    /// Get all discriminators in order
    pub(crate) fn discriminators(&self) -> Vec<u8> {
        let mut discriminators: Vec<u8> = self.types.keys().copied().collect();
        discriminators.sort_unstable();
        discriminators
    }
}

pub(crate) struct VariantDeserializer;

impl VariantDeserializer {
    /// Build offsets and count rows for each discriminator type
    fn build_offsets_and_counts(
        discriminators: &[u8],
        rows: usize,
    ) -> (Vec<usize>, HashMap<u8, usize>) {
        let mut offsets = vec![0; rows];
        let mut row_count_by_type: HashMap<u8, usize> = HashMap::new();

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != 0xFF {
                let count = row_count_by_type.entry(disc).or_insert(0);
                offsets[i] = *count;
                *count += 1;
            }
        }

        (offsets, row_count_by_type)
    }

    /// Reconstruct values in original order from column data
    fn reconstruct_values(
        discriminators: &[u8],
        offsets: &[usize],
        columns: &HashMap<u8, Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(discriminators.len());

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc == 0xFF {
                values.push(Value::Variant(disc, Box::new(Value::Null)));
            } else if let Some(column) = columns.get(&disc) {
                let offset = offsets[i];
                if offset < column.len() {
                    values.push(Value::Variant(disc, Box::new(column[offset].clone())));
                } else {
                    return Err(crate::Error::DeserializeError(format!(
                        "Invalid offset {} for discriminator {}",
                        offset, disc
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

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // Read version prefix (8 bytes, should be 0)
        let version = reader.read_u64_le().await?;
        if version != 0 {
            return Err(crate::Error::DeserializeError(format!(
                "Unsupported Variant serialization version: {}",
                version
            )));
        }

        // Read prefixes for nested types that require them
        let variant_types = type_.unwrap_variant()?;
        for inner_type in variant_types {
            inner_type.deserialize_prefix_async(reader, state).await?;
        }

        Ok(())
    }

    pub(crate) async fn read_async<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map = DiscriminatorMap::new(variant_types)?;

        // Read discriminators as a simple byte array
        let mut discriminators = vec![0u8; rows];
        let _ = reader.read_exact(&mut discriminators).await?;

        // Build offsets and count rows per type
        let (offsets, row_count_by_type) = Self::build_offsets_and_counts(&discriminators, rows);

        // Read the column data for each type in discriminator order
        let mut columns: HashMap<u8, Vec<Value>> = HashMap::new();

        for discriminator in discriminator_map.discriminators() {
            if let Some(&count) = row_count_by_type.get(&discriminator) {
                if count > 0 {
                    if let Some(inner_type) = discriminator_map.get_type(discriminator) {
                        let column_values =
                            inner_type.deserialize_column(reader, count, state).await?;
                        drop(columns.insert(discriminator, column_values));
                    }
                }
            }
        }

        // Reconstruct the values in original order
        Self::reconstruct_values(&discriminators, &offsets, &columns)
    }

    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
    ) -> Result<()> {
        // Read version prefix (8 bytes, should be 0)
        let version = reader.get_u64_le();
        if version != 0 {
            return Err(crate::Error::DeserializeError(format!(
                "Unsupported Variant serialization version: {}",
                version
            )));
        }

        // Read prefixes for nested types that require them
        let variant_types = type_.unwrap_variant()?;
        for inner_type in variant_types {
            inner_type.deserialize_prefix(reader)?;
        }

        Ok(())
    }

    pub(crate) fn read_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Sanity check
        if rows > 1_000_000 {
            return Err(crate::Error::DeserializeError(format!(
                "Variant row count too large: {} (likely corrupt data)",
                rows
            )));
        }

        let variant_types = type_.unwrap_variant()?;
        let discriminator_map = DiscriminatorMap::new(variant_types)?;

        // Read discriminators as a simple byte array
        let mut discriminators = vec![0u8; rows];
        reader.try_copy_to_slice(&mut discriminators)?;

        // Build offsets and count rows per type
        let (offsets, row_count_by_type) = Self::build_offsets_and_counts(&discriminators, rows);

        // Read the column data for each type in discriminator order
        let mut columns: HashMap<u8, Vec<Value>> = HashMap::new();

        for discriminator in discriminator_map.discriminators() {
            if let Some(&count) = row_count_by_type.get(&discriminator) {
                if count > 0 {
                    if let Some(inner_type) = discriminator_map.get_type(discriminator) {
                        let column_values =
                            inner_type.deserialize_column_sync(reader, count, state)?;
                        drop(columns.insert(discriminator, column_values));
                    }
                }
            }
        }

        // Reconstruct the values in original order
        Self::reconstruct_values(&discriminators, &offsets, &columns)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;

    #[test]
    fn test_discriminator_map_sorting() {
        // Test that types are sorted alphabetically
        let types = vec![Type::String, Type::UInt64, Type::Array(Box::new(Type::String))];

        let map = DiscriminatorMap::new(&types).unwrap();

        // Expected order: Array(String), String, UInt64
        assert_eq!(map.get_type(0).unwrap().to_string(), "Array(String)");
        assert_eq!(map.get_type(1).unwrap().to_string(), "String");
        assert_eq!(map.get_type(2).unwrap().to_string(), "UInt64");
        assert!(map.get_type(3).is_none());
    }

    #[test]
    fn test_discriminator_map_date_datetime() {
        // Test sorting with Date, DateTime, and String
        let types = vec![Type::String, Type::DateTime(chrono_tz::UTC), Type::Date];

        let map = DiscriminatorMap::new(&types).unwrap();

        // Expected order: Date, DateTime('UTC'), String
        assert_eq!(map.get_type(0).unwrap().to_string(), "Date");
        assert_eq!(map.get_type(1).unwrap().to_string(), "DateTime('UTC')");
        assert_eq!(map.get_type(2).unwrap().to_string(), "String");
    }

    #[test]
    fn test_variant_simple_deserialization() {
        // Test Variant(String, UInt64)
        // Based on TCP dump: multiif returns 'yes' (String), 2 (UInt64), 'yes' (String)
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        // Discriminators: String=0, UInt64=1 (alphabetically sorted)
        // Values: 'yes' -> 0, 2 -> 1, 'yes' -> 0
        let data = vec![
            // Version prefix (8 bytes of 0)
            0u8, 0, 0, 0, 0, 0, 0, 0, // Discriminators
            0u8, 1u8, 0u8, // String data (2 rows of 'yes')
            3, b'y', b'e', b's', // 'yes'
            3, b'y', b'e', b's', // 'yes'
            // UInt64 data (1 row of value 2)
            2, 0, 0, 0, 0, 0, 0, 0, // 2 as UInt64
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        variant_type.deserialize_prefix(&mut reader).unwrap();

        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut state).unwrap();

        assert_eq!(values.len(), 3);

        // Check first value: 'yes'
        match &values[0] {
            Value::Variant(0, inner) => {
                assert_eq!(**inner, Value::String(b"yes".to_vec()));
            }
            _ => panic!("Expected Variant(0, String('yes')), got {:?}", values[0]),
        }

        // Check second value: 2
        match &values[1] {
            Value::Variant(1, inner) => {
                assert_eq!(**inner, Value::UInt64(2));
            }
            _ => panic!("Expected Variant(1, UInt64(2)), got {:?}", values[1]),
        }

        // Check third value: 'yes'
        match &values[2] {
            Value::Variant(0, inner) => {
                assert_eq!(**inner, Value::String(b"yes".to_vec()));
            }
            _ => panic!("Expected Variant(0, String('yes')), got {:?}", values[2]),
        }
    }

    #[test]
    fn test_variant_with_nulls() {
        // Test Variant(String, UInt64) with NULL values
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        // Discriminators: String=0, UInt64=1, NULL=0xFF
        let data = vec![
            // Version prefix (8 bytes of 0)
            0u8, 0, 0, 0, 0, 0, 0, 0, // Discriminators
            0u8, 0xFF, 1u8, // String data (1 row)
            5, b'h', b'e', b'l', b'l', b'o', // 'hello'
            // UInt64 data (1 row)
            42, 0, 0, 0, 0, 0, 0, 0, // 42 as UInt64
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        variant_type.deserialize_prefix(&mut reader).unwrap();

        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut state).unwrap();

        assert_eq!(values.len(), 3);

        // Check first value: 'hello'
        match &values[0] {
            Value::Variant(0, inner) => {
                assert_eq!(**inner, Value::String(b"hello".to_vec()));
            }
            _ => panic!("Expected Variant(0, String('hello')), got {:?}", values[0]),
        }

        // Check second value: NULL
        match &values[1] {
            Value::Variant(0xFF, inner) => {
                assert_eq!(**inner, Value::Null);
            }
            _ => panic!("Expected Variant(0xFF, Null), got {:?}", values[1]),
        }

        // Check third value: 42
        match &values[2] {
            Value::Variant(1, inner) => {
                assert_eq!(**inner, Value::UInt64(42));
            }
            _ => panic!("Expected Variant(1, UInt64(42)), got {:?}", values[2]),
        }
    }

    #[test]
    fn test_variant_with_complex_types() {
        // Test Variant(Array(String), UInt64, Date)
        let variant_type =
            Type::Variant(vec![Type::Array(Box::new(Type::String)), Type::UInt64, Type::Date]);

        // Discriminators sorted: Array(String)=0, Date=1, UInt64=2
        // Test data: [['a', 'b']], 2024-01-01, 42
        let data = vec![
            // Version prefix (8 bytes of 0)
            0u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            // Discriminators
            0u8,
            1u8,
            2u8,
            // Array(String) data (1 row)
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // offset 2 (array has 2 elements)
            1,
            b'a', // 'a'
            1,
            b'b', // 'b'
            // Date data (1 row) - days since 1970-01-01
            19723u16.to_le_bytes()[0],
            19723u16.to_le_bytes()[1], // 2024-01-01
            // UInt64 data (1 row)
            42,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // 42
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        variant_type.deserialize_prefix(&mut reader).unwrap();

        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut state).unwrap();

        assert_eq!(values.len(), 3);

        // Check first value: ['a', 'b']
        match &values[0] {
            Value::Variant(0, inner) => match &**inner {
                Value::Array(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0], Value::String(b"a".to_vec()));
                    assert_eq!(items[1], Value::String(b"b".to_vec()));
                }
                _ => panic!("Expected Array, got {:?}", inner),
            },
            _ => panic!("Expected Variant(0, Array), got {:?}", values[0]),
        }

        // Check second value: Date
        match &values[1] {
            Value::Variant(1, inner) => {
                assert_eq!(**inner, Value::Date(crate::native::values::Date(19723))); // 2024-01-01
            }
            _ => panic!("Expected Variant(1, Date), got {:?}", values[1]),
        }

        // Check third value: 42
        match &values[2] {
            Value::Variant(2, inner) => {
                assert_eq!(**inner, Value::UInt64(42));
            }
            _ => panic!("Expected Variant(2, UInt64(42)), got {:?}", values[2]),
        }
    }

    #[test]
    fn test_variant_multitype_sorting() {
        // Test with 5 types to verify alphabetical sorting
        let variant_type = Type::Variant(vec![
            Type::UInt64,
            Type::String,
            Type::Date,
            Type::Array(Box::new(Type::UInt8)),
            Type::DateTime(chrono_tz::UTC),
        ]);

        // Expected discriminator mapping (alphabetically sorted):
        // Array(UInt8)=0, Date=1, DateTime('UTC')=2, String=3, UInt64=4
        let data = vec![
            // Version prefix (8 bytes of 0)
            0u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            // Discriminators: UInt64, String, Date, Array(UInt8), DateTime
            4u8,
            3u8,
            1u8,
            0u8,
            2u8,
            // Array(UInt8) data (1 row)
            3,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // offset 3 (array has 3 elements)
            1,
            2,
            3, // values [1, 2, 3]
            // Date data (1 row)
            100u16.to_le_bytes()[0],
            100u16.to_le_bytes()[1], // day 100
            // DateTime data (1 row)
            1_234_567_890_u32.to_le_bytes()[0],
            1_234_567_890_u32.to_le_bytes()[1],
            1_234_567_890_u32.to_le_bytes()[2],
            1_234_567_890_u32.to_le_bytes()[3], // Unix timestamp
            // String data (1 row)
            5,
            b'h',
            b'e',
            b'l',
            b'l',
            b'o', // 'hello'
            // UInt64 data (1 row)
            231,
            3,
            0,
            0,
            0,
            0,
            0,
            0, // 999 (0x03E7 in little-endian)
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        variant_type.deserialize_prefix(&mut reader).unwrap();

        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 5, &mut state).unwrap();

        assert_eq!(values.len(), 5);

        // Check values in original order
        // First: UInt64(999) -> discriminator 4
        match &values[0] {
            Value::Variant(4, inner) => {
                assert_eq!(**inner, Value::UInt64(999));
            }
            _ => panic!("Expected Variant(4, UInt64(999)), got {:?}", values[0]),
        }

        // Second: String('hello') -> discriminator 3
        match &values[1] {
            Value::Variant(3, inner) => {
                assert_eq!(**inner, Value::String(b"hello".to_vec()));
            }
            _ => panic!("Expected Variant(3, String('hello')), got {:?}", values[1]),
        }

        // Third: Date(100) -> discriminator 1
        match &values[2] {
            Value::Variant(1, inner) => {
                assert_eq!(**inner, Value::Date(crate::native::values::Date(100)));
            }
            _ => panic!("Expected Variant(1, Date(100)), got {:?}", values[2]),
        }

        // Fourth: Array([1,2,3]) -> discriminator 0
        match &values[3] {
            Value::Variant(0, inner) => match &**inner {
                Value::Array(items) => {
                    assert_eq!(items.len(), 3);
                    assert_eq!(items[0], Value::UInt8(1));
                    assert_eq!(items[1], Value::UInt8(2));
                    assert_eq!(items[2], Value::UInt8(3));
                }
                _ => panic!("Expected Array, got {:?}", inner),
            },
            _ => panic!("Expected Variant(0, Array), got {:?}", values[3]),
        }

        // Fifth: DateTime -> discriminator 2
        match &values[4] {
            Value::Variant(2, inner) => {
                match &**inner {
                    Value::DateTime(dt) => {
                        assert_eq!(dt.1, 1_234_567_890); // Check timestamp
                    }
                    _ => panic!("Expected DateTime, got {:?}", inner),
                }
            }
            _ => panic!("Expected Variant(2, DateTime), got {:?}", values[4]),
        }
    }

    #[test]
    fn test_nested_variant_parsing() {
        use std::str::FromStr;

        // Test basic nested variant parsing
        let nested_str = "Variant(String, Variant(UInt64, Date))";
        let nested_type = Type::from_str(nested_str).unwrap();

        // Check it parsed correctly
        match &nested_type {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0].to_string(), "String");
                // The inner variant was parsed correctly - check its structure
                match &types[1] {
                    Type::Variant(inner_types) => {
                        assert_eq!(inner_types.len(), 2);
                        // Contains UInt64 and Date (order doesn't matter for parsing)
                        assert!(inner_types.iter().any(|t| matches!(t, Type::UInt64)));
                        assert!(inner_types.iter().any(|t| matches!(t, Type::Date)));
                    }
                    _ => panic!("Expected inner type to be Variant"),
                }
            }
            _ => panic!("Expected Variant type"),
        }

        // Test discriminator mapping for outer variant
        let map = DiscriminatorMap::new(match &nested_type {
            Type::Variant(types) => types,
            _ => panic!("Expected Variant"),
        })
        .unwrap();

        // String comes before Variant alphabetically
        assert_eq!(map.get_type(0).unwrap().to_string(), "String");
        assert!(matches!(map.get_type(1).unwrap(), Type::Variant(_)));
    }

    #[test]
    #[ignore = "ClickHouse doesn't support nested Variant types"]
    fn test_recursive_variant_deserialization() {
        // Test Variant(String, Variant(UInt64, String))
        // Note: ClickHouse actually doesn't allow nested Variant types
        // This test is kept for completeness but ignored
        let outer_type =
            Type::Variant(vec![Type::String, Type::Variant(vec![Type::UInt64, Type::String])]);

        // Expected discriminators:
        // Outer: String=0, Variant(UInt64, String)=1
        // Inner: String=0, UInt64=1 (alphabetical)

        // Test data: "hello" (outer String), then inner variant with 42 (UInt64)
        // Inner variant has types [UInt64, String] which sorts to [String, UInt64]
        // So String=0, UInt64=1

        // When running the test against real server to understand the format:
        // let's construct the data as it would appear from the server
        let data = vec![
            // Version prefix for outer variant (8 bytes of 0)
            0u8, 0, 0, 0, 0, 0, 0, 0, // Outer discriminators
            0u8, 1u8, // String data for outer (1 row)
            5, b'h', b'e', b'l', b'l', b'o', // "hello"
            // Inner variant data (1 row)
            // When Variant is nested, it still gets its own version prefix!
            0u8, 0, 0, 0, 0, 0, 0, 0,   // Version prefix for inner variant
            1u8, // Inner discriminator for UInt64
            // No String data for inner variant (0 rows with discriminator 0)
            // UInt64 data for inner variant (1 row)
            42, 0, 0, 0, 0, 0, 0, 0, // 42
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        outer_type.deserialize_prefix(&mut reader).unwrap();

        let values =
            VariantDeserializer::read_sync(&outer_type, &mut reader, 2, &mut state).unwrap();

        assert_eq!(values.len(), 2);

        // First value: "hello"
        match &values[0] {
            Value::Variant(0, inner) => {
                assert_eq!(**inner, Value::String(b"hello".to_vec()));
            }
            _ => panic!("Expected Variant(0, String), got {:?}", values[0]),
        }

        // Second value: inner variant containing UInt64(42)
        match &values[1] {
            Value::Variant(1, inner) => {
                // This should be a variant value
                match &**inner {
                    Value::Variant(1, inner_inner) => {
                        assert_eq!(**inner_inner, Value::UInt64(42));
                    }
                    _ => panic!("Expected inner Variant(1, UInt64), got {:?}", inner),
                }
            }
            _ => panic!("Expected Variant(1, Variant), got {:?}", values[1]),
        }
    }

    #[tokio::test]
    async fn test_variant_async_deserialization() {
        // Test async version with Variant(String, UInt64)
        let variant_type = Type::Variant(vec![Type::String, Type::UInt64]);

        // Since discriminators are sorted: String=0, UInt64=1
        // But data is sent in discriminator order: 0 first, then 1
        let data = vec![
            // Version prefix (8 bytes of 0)
            0u8, 0, 0, 0, 0, 0, 0, 0, // Discriminators
            1u8, 0u8, // String data first (1 row for discriminator 0)
            4, b't', b'e', b's', b't', // 'test'
            // UInt64 data next (1 row for discriminator 1)
            100, 0, 0, 0, 0, 0, 0, 0, // 100 as UInt64
        ];

        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();

        // Read prefix first
        use crate::native::types::deserialize::ClickHouseNativeDeserializer;
        variant_type.deserialize_prefix_async(&mut reader, &mut state).await.unwrap();

        let values = VariantDeserializer::read_async(&variant_type, &mut reader, 2, &mut state)
            .await
            .unwrap();

        assert_eq!(values.len(), 2);

        // Check first value: 100
        match &values[0] {
            Value::Variant(1, inner) => {
                assert_eq!(**inner, Value::UInt64(100));
            }
            _ => panic!("Expected Variant(1, UInt64(100)), got {:?}", values[0]),
        }

        // Check second value: 'test'
        match &values[1] {
            Value::Variant(0, inner) => {
                assert_eq!(**inner, Value::String(b"test".to_vec()));
            }
            _ => panic!("Expected Variant(0, String('test')), got {:?}", values[1]),
        }
    }
}
