use std::collections::{BTreeMap, HashMap};

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::types::deserialize::{ClickHouseNativeDeserializer, DeserializerState};
use crate::native::types::{Type, Value};

const NULL_DISCRIMINATOR: u8 = 0xFF;
const MAX_VARIANT_ROWS: usize = 1_000_000;

/// Represents a mapping from discriminator values to types and their string representations
#[derive(Debug, Clone)]
pub(crate) struct DiscriminatorMap {
    /// Maps discriminator byte to (`type_string`, `type`)
    types:                 HashMap<u8, (String, Type)>,
    /// Sorted discriminators for iteration
    sorted_discriminators: Vec<u8>,
}

impl DiscriminatorMap {
    /// Create a new discriminator map from variant types
    pub(crate) fn new(variant_types: &[Type]) -> Self {
        // Use BTreeMap to maintain sorted order automatically
        let mut sorted_types = BTreeMap::new();
        for t in variant_types {
            let old = sorted_types.insert(t.to_string(), t.clone());
            debug_assert!(old.is_none(), "Duplicate type in variant");
        }

        // Build discriminator map with automatic sorting
        let mut types = HashMap::with_capacity(sorted_types.len());
        let mut sorted_discriminators = Vec::with_capacity(sorted_types.len());

        for (idx, (type_str, type_)) in sorted_types.into_iter().enumerate() {
            let discriminator = u8::try_from(idx).expect("Too many variant types");
            let old = types.insert(discriminator, (type_str, type_));
            debug_assert!(old.is_none(), "Duplicate discriminator");
            sorted_discriminators.push(discriminator);
        }

        Self { types, sorted_discriminators }
    }

    /// Get the type for a given discriminator
    #[inline]
    pub(crate) fn get_type(&self, discriminator: u8) -> Option<&Type> {
        self.types.get(&discriminator).map(|(_, t)| t)
    }

    /// Get all discriminators in order
    #[inline]
    pub(crate) fn discriminators(&self) -> &[u8] { &self.sorted_discriminators }
}

pub(crate) struct VariantDeserializer;

// Macro to implement version check
macro_rules! check_version {
    ($version:expr) => {
        if $version != 0 {
            return Err(crate::Error::DeserializeError(format!(
                "Unsupported Variant serialization version: {}",
                $version
            )));
        }
    };
}

impl VariantDeserializer {
    /// Build offsets and count rows for each discriminator type
    fn build_offsets_and_counts(
        discriminators: &[u8],
        rows: usize,
    ) -> (Vec<usize>, HashMap<u8, usize>) {
        let mut offsets = vec![0; rows];
        let mut row_count_by_type = HashMap::new();

        for (i, &disc) in discriminators.iter().enumerate() {
            if disc != NULL_DISCRIMINATOR {
                let count = row_count_by_type.entry(disc).or_default();
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
        discriminators
            .iter()
            .zip(offsets)
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

    /// Read Variant data (async version)
    async fn read_internal_async<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let variant_types = type_.unwrap_variant()?;
        let discriminator_map = DiscriminatorMap::new(variant_types);

        // Read discriminators
        let mut discriminators = vec![0u8; rows];
        let _ = reader.read_exact(&mut discriminators).await?;

        // Build offsets and count rows per type
        let (offsets, row_count_by_type) = Self::build_offsets_and_counts(&discriminators, rows);

        // Read column data for each type
        let mut columns = HashMap::new();

        for &discriminator in discriminator_map.discriminators() {
            if let Some(&count) = row_count_by_type.get(&discriminator)
                && count > 0
                && let Some(inner_type) = discriminator_map.get_type(discriminator)
            {
                let column_values = inner_type.deserialize_column(reader, count, state).await?;
                let old = columns.insert(discriminator, column_values);
                debug_assert!(old.is_none(), "Duplicate discriminator column");
            }
        }

        // Reconstruct values in original order
        Self::reconstruct_values(&discriminators, &offsets, &columns)
    }

    /// Read Variant data (sync version)
    fn read_internal_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Sanity check
        if rows > MAX_VARIANT_ROWS {
            return Err(crate::Error::DeserializeError(format!(
                "Variant row count too large: {rows} (likely corrupt data)"
            )));
        }

        let variant_types = type_.unwrap_variant()?;
        let discriminator_map = DiscriminatorMap::new(variant_types);

        // Read discriminators
        let mut discriminators = vec![0u8; rows];
        reader.try_copy_to_slice(&mut discriminators)?;

        // Build offsets and count rows per type
        let (offsets, row_count_by_type) = Self::build_offsets_and_counts(&discriminators, rows);

        // Read column data for each type
        let mut columns = HashMap::new();

        for &discriminator in discriminator_map.discriminators() {
            if let Some(&count) = row_count_by_type.get(&discriminator)
                && count > 0
                && let Some(inner_type) = discriminator_map.get_type(discriminator)
            {
                let column_values = inner_type.deserialize_column_sync(reader, count, state)?;
                let old = columns.insert(discriminator, column_values);
                debug_assert!(old.is_none(), "Duplicate discriminator column");
            }
        }

        // Reconstruct values in original order
        Self::reconstruct_values(&discriminators, &offsets, &columns)
    }

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // Read version. Typed-path prefixes are read via the nested serializer using a fresh
        // DeserializerState to avoid leaking per-path state.
        let version = reader.read_u64_le().await?;
        check_version!(version);

        // Read prefixes for nested types
        for inner_type in type_.unwrap_variant()? {
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
        Self::read_internal_async(type_, reader, rows, state).await
    }

    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
    ) -> Result<()> {
        // Always read version here; callers only skip if they explicitly mark JSON data context.
        let version = reader.get_u64_le();
        check_version!(version);

        // Read prefixes for nested types
        for inner_type in type_.unwrap_variant()? {
            inner_type.deserialize_prefix(reader, &mut DeserializerState::default())?;
        }

        Ok(())
    }

    pub(crate) fn read_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        Self::read_internal_sync(type_, reader, rows, state)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;

    // Macro for variant value assertions
    macro_rules! assert_variant {
        ($value:expr, $disc:expr, $expected:expr) => {
            match $value {
                Value::Variant(disc, inner) if disc == &$disc => {
                    assert_eq!(**inner, $expected, "Inner value mismatch");
                }
                _ => panic!("Expected Variant({}, {:?}), got {:?}", $disc, $expected, $value),
            }
        };
    }

    // Helper to create test data with version prefix
    fn create_test_data(discriminators: &[u8], data: &[u8]) -> Vec<u8> {
        let mut result = vec![0u8; 8]; // Version prefix
        result.extend_from_slice(discriminators);
        result.extend_from_slice(data);
        result
    }

    /// Macro to test variant deserialization for both sync and async paths
    macro_rules! variant_deserialization_test {
        (
            $name:ident,
            $variant_type:expr,
            $discriminators:expr,
            $data:expr,
            $expected_values:expr
        ) => {
            #[tokio::test]
            async fn $name() {
                let variant_type = $variant_type;
                let discriminators = $discriminators;
                let data = $data;
                let expected_values = $expected_values;

                let test_data = create_test_data(discriminators, data);

                // Test sync path
                let mut sync_reader = Cursor::new(test_data.clone());
                let mut sync_state = DeserializerState::default();
                variant_type
                    .deserialize_prefix(&mut sync_reader, &mut DeserializerState::default())
                    .unwrap();
                let sync_values = VariantDeserializer::read_sync(
                    &variant_type,
                    &mut sync_reader,
                    discriminators.len(),
                    &mut sync_state,
                )
                .unwrap();

                // Test async path
                let mut async_reader = Cursor::new(test_data);
                let mut async_state = DeserializerState::default();
                VariantDeserializer::read_prefix(
                    &variant_type,
                    &mut async_reader,
                    &mut async_state,
                )
                .await
                .unwrap();
                let async_values = VariantDeserializer::read_async(
                    &variant_type,
                    &mut async_reader,
                    discriminators.len(),
                    &mut async_state,
                )
                .await
                .unwrap();

                // Assert both paths produce the same results
                assert_eq!(sync_values.len(), expected_values.len());
                assert_eq!(async_values.len(), expected_values.len());
                assert_eq!(sync_values, async_values, "Sync and async results should match");

                for (i, (expected_disc, expected_val)) in expected_values.iter().enumerate() {
                    assert_variant!(&sync_values[i], *expected_disc, expected_val.clone());
                    assert_variant!(&async_values[i], *expected_disc, expected_val.clone());
                }
            }
        };
    }

    // Helper function to create multitype test data programmatically
    fn create_multitype_test_data() -> (Type, Vec<u8>) {
        let variant_type = Type::variant(vec![
            Type::UInt64,
            Type::String,
            Type::Date,
            Type::Array(Box::new(Type::UInt8)),
            Type::DateTime(chrono_tz::UTC),
        ]);

        let mut data = vec![0u8; 8]; // Version prefix
        data.extend_from_slice(&[4u8, 3u8, 1u8, 0u8, 2u8]); // Discriminators
        // Array(UInt8) data
        data.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]);
        // Date data
        data.extend_from_slice(&100u16.to_le_bytes());
        // DateTime data
        data.extend_from_slice(&1_234_567_890_u32.to_le_bytes());
        // String data
        data.extend_from_slice(&[5, b'h', b'e', b'l', b'l', b'o']);
        // UInt64 data
        data.extend_from_slice(&[231, 3, 0, 0, 0, 0, 0, 0]); // 999

        (variant_type, data)
    }

    // Discriminator map tests
    #[test]
    fn test_discriminator_map_basic_sorting() {
        let types = vec![Type::String, Type::UInt64, Type::Array(Box::new(Type::String))];
        let map = DiscriminatorMap::new(&types);
        // Expected order: Array(String), String, UInt64
        assert_eq!(map.get_type(0).unwrap().to_string(), "Array(String)");
        assert_eq!(map.get_type(1).unwrap().to_string(), "String");
        assert_eq!(map.get_type(2).unwrap().to_string(), "UInt64");
        assert!(map.get_type(3).is_none());
    }

    #[test]
    fn test_discriminator_map_datetime_sorting() {
        let types = vec![Type::String, Type::DateTime(chrono_tz::UTC), Type::Date];
        let map = DiscriminatorMap::new(&types);
        // Expected order: Date, DateTime('UTC'), String
        assert_eq!(map.get_type(0).unwrap().to_string(), "Date");
        assert_eq!(map.get_type(1).unwrap().to_string(), "DateTime('UTC')");
        assert_eq!(map.get_type(2).unwrap().to_string(), "String");
    }

    #[test]
    fn test_discriminator_map_nested_variant() {
        use std::str::FromStr;
        let nested_str = "Variant(String, Variant(UInt64, Date))";
        let nested_type = Type::from_str(nested_str).unwrap();
        match &nested_type {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0].to_string(), "String");
                assert!(matches!(&types[1], Type::Variant(_)));
            }
            _ => panic!("Expected Variant type"),
        }
        let nested_map = DiscriminatorMap::new(match &nested_type {
            Type::Variant(types) => types,
            _ => unreachable!(),
        });
        assert_eq!(nested_map.get_type(0).unwrap().to_string(), "String");
        assert!(matches!(nested_map.get_type(1).unwrap(), Type::Variant(_)));
    }

    // Use the macro to create sync/async test pairs
    variant_deserialization_test!(
        test_variant_simple_deserialization,
        Type::variant(vec![Type::String, Type::UInt64]),
        &[0u8, 1u8, 0u8],
        &[3, b'y', b'e', b's', 3, b'y', b'e', b's', 2, 0, 0, 0, 0, 0, 0, 0],
        &[
            (0, Value::String(b"yes".to_vec())),
            (1, Value::UInt64(2)),
            (0, Value::String(b"yes".to_vec())),
        ]
    );

    variant_deserialization_test!(
        test_variant_null_deserialization,
        Type::variant(vec![Type::String, Type::UInt64]),
        &[0u8, 0xFF, 1u8],
        &[5, b'h', b'e', b'l', b'l', b'o', 42, 0, 0, 0, 0, 0, 0, 0],
        &[(0, Value::String(b"hello".to_vec())), (0xFF, Value::Null), (1, Value::UInt64(42))]
    );

    #[test]
    fn test_variant_complex_array_deserialization() {
        let variant_type =
            Type::variant(vec![Type::Array(Box::new(Type::String)), Type::UInt64, Type::Date]);
        let date_bytes = 19723u16.to_le_bytes();
        let data = create_test_data(&[0u8, 1u8, 2u8], &[
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // offset 2
            1,
            b'a', // 'a'
            1,
            b'b', // 'b'
            date_bytes[0],
            date_bytes[1], // Date
            42,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // 42
        ]);
        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();
        variant_type.deserialize_prefix(&mut reader, &mut state).unwrap();
        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 3, &mut state).unwrap();
        assert_eq!(values.len(), 3);

        // Check array value
        match &values[0] {
            Value::Variant(0, inner) => match &**inner {
                Value::Array(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0], Value::String(b"a".to_vec()));
                    assert_eq!(items[1], Value::String(b"b".to_vec()));
                }
                _ => panic!("Expected Array"),
            },
            _ => panic!("Expected Variant(0, Array)"),
        }
        assert_variant!(&values[1], 1, Value::Date(crate::native::values::Date(19723)));
        assert_variant!(&values[2], 2, Value::UInt64(42));
    }

    // Multitype sorting tests
    #[test]
    fn test_variant_multitype_discriminator_order() {
        let (variant_type, data) = create_multitype_test_data();
        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();
        variant_type.deserialize_prefix(&mut reader, &mut state).unwrap();
        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 5, &mut state).unwrap();
        assert_eq!(values.len(), 5);

        // Verify discriminator assignments match expected sort order
        assert_variant!(&values[0], 4, Value::UInt64(999)); // UInt64 -> discriminator 4
        assert_variant!(&values[1], 3, Value::String(b"hello".to_vec())); // String -> discriminator 3
        assert_variant!(&values[2], 1, Value::Date(crate::native::values::Date(100))); // Date -> discriminator 1
    }

    #[test]
    fn test_variant_multitype_array_handling() {
        let (variant_type, data) = create_multitype_test_data();
        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();
        variant_type.deserialize_prefix(&mut reader, &mut state).unwrap();
        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 5, &mut state).unwrap();

        // Check array (discriminator 0)
        match &values[3] {
            Value::Variant(0, inner) => match &**inner {
                Value::Array(items) => {
                    assert_eq!(items, &[Value::UInt8(1), Value::UInt8(2), Value::UInt8(3)]);
                }
                _ => panic!("Expected Array"),
            },
            _ => panic!("Expected Variant(0, Array)"),
        }
    }

    #[test]
    fn test_variant_multitype_datetime_handling() {
        let (variant_type, data) = create_multitype_test_data();
        let mut reader = Cursor::new(data);
        let mut state = DeserializerState::default();
        variant_type.deserialize_prefix(&mut reader, &mut state).unwrap();
        let values =
            VariantDeserializer::read_sync(&variant_type, &mut reader, 5, &mut state).unwrap();

        // Check DateTime (discriminator 2)
        match &values[4] {
            Value::Variant(2, inner) => match &**inner {
                Value::DateTime(dt) => assert_eq!(dt.1, 1_234_567_890),
                _ => panic!("Expected DateTime"),
            },
            _ => panic!("Expected Variant(2, DateTime)"),
        }
    }

    // Use the macro for async tests too
    variant_deserialization_test!(
        test_variant_async_basic_deserialization,
        Type::variant(vec![Type::String, Type::UInt64]),
        &[1u8, 0u8],
        &[4, b't', b'e', b's', b't', 100, 0, 0, 0, 0, 0, 0, 0],
        &[(1, Value::UInt64(100)), (0, Value::String(b"test".to_vec()))]
    );

    variant_deserialization_test!(
        test_variant_async_different_types,
        Type::variant(vec![Type::String, Type::UInt32]),
        &[0u8, 1u8],
        &[
            4, b't', b'e', b's', b't', // 'test'
            50, 0, 0, 0, // 50 as UInt32
        ],
        &[(0, Value::String(b"test".to_vec())), (1, Value::UInt32(50))]
    );
}
