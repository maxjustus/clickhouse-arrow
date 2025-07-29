use chrono::NaiveDate;
use chrono_tz::Tz;
use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::{ColumnDefinition, Type, Value};
use uuid::Uuid;

/// Test data for round trip
#[derive(Debug, Default, PartialEq, Clone)]
#[cfg_attr(feature = "derive", derive(Row))]
#[cfg_attr(feature = "derive", clickhouse_arrow(schema = get_testrowall_schema))]
pub struct TestRowAll {
    id:                        u64,
    int8_col:                  i8,
    int16_col:                 i16,
    int32_col:                 i32,
    int64_col:                 i64,
    uint8_col:                 u8,
    uint16_col:                u16,
    uint32_col:                u32,
    uint64_col:                u64,
    uint128_col:               u128,
    uint256_col:               u256,
    date_col:                  Date,
    datetime_col:              DateTime,
    datetime64_col:            DateTime64<3>,
    #[cfg(feature = "rust_decimal")]
    decimal32_col:             rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal32_col:             FixedPoint32<4>,
    #[cfg(feature = "rust_decimal")]
    decimal64_col:             rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal64_col:             FixedPoint64<6>,
    #[cfg(feature = "rust_decimal")]
    decimal128_col:            rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal128_col:            FixedPoint128<8>,
    decimal256_col:            FixedPoint256<10>,
    nullable_string_col:       Option<String>,
    nullable_int32_col:        Option<i32>,
    nullable_uint64_col:       Option<u64>,
    array_uint64_col:          Vec<u64>,
    array_string_col:          Vec<String>,
    array_nullable_int32_col:  Vec<Option<i32>>,
    array_nullable_string_col: Vec<Option<String>>,
    string_col:                String,
    fixed_string_col:          String, // FixedString(5) trimmed to String
    uuid_col:                  Uuid,
}

pub fn get_testrowall_schema() -> Vec<ColumnDefinition> {
    vec![
        ("id".to_string(), Type::UInt64, None),
        ("int8_col".to_string(), Type::Int8, None),
        ("int16_col".to_string(), Type::Int16, None),
        ("int32_col".to_string(), Type::Int32, None),
        ("int64_col".to_string(), Type::Int64, None),
        ("uint8_col".to_string(), Type::UInt8, None),
        ("uint16_col".to_string(), Type::UInt16, None),
        ("uint32_col".to_string(), Type::UInt32, None),
        ("uint64_col".to_string(), Type::UInt64, None),
        ("uint128_col".to_string(), Type::UInt128, None),
        ("uint256_col".to_string(), Type::UInt256, None),
        ("date_col".to_string(), Type::Date, None),
        ("datetime_col".to_string(), Type::DateTime(Tz::UTC), None),
        ("datetime64_col".to_string(), Type::DateTime64(3, Tz::UTC), None),
        ("decimal32_col".to_string(), Type::Decimal32(4), None),
        ("decimal64_col".to_string(), Type::Decimal64(6), None),
        ("decimal128_col".to_string(), Type::Decimal128(8), None),
        ("decimal256_col".to_string(), Type::Decimal256(10), None),
        ("nullable_string_col".to_string(), Type::Nullable(Box::new(Type::String)), None),
        ("nullable_int32_col".to_string(), Type::Nullable(Box::new(Type::Int32)), None),
        ("nullable_uint64_col".to_string(), Type::Nullable(Box::new(Type::UInt64)), None),
        ("array_uint64_col".to_string(), Type::Array(Box::new(Type::UInt64)), None),
        ("array_string_col".to_string(), Type::Array(Box::new(Type::String)), None),
        (
            "array_nullable_int32_col".to_string(),
            Type::Array(Box::new(Type::Nullable(Box::new(Type::Int32)))),
            None,
        ),
        (
            "array_nullable_string_col".to_string(),
            Type::Array(Box::new(Type::Nullable(Box::new(Type::String)))),
            None,
        ),
        ("string_col".to_string(), Type::String, None),
        ("fixed_string_col".to_string(), Type::FixedSizedString(5), None),
        ("uuid_col".to_string(), Type::Uuid, None),
    ]
}

/// # Panics
#[expect(clippy::too_many_lines)]
pub fn generate_test_block() -> Vec<TestRowAll> {
    use clickhouse_arrow::i256;

    vec![
        // Row 1: Basic values with all non-nullable fields populated and nullable fields Some
        TestRowAll {
            id: 1,
            int8_col: 8,
            int16_col: 16,
            int32_col: 32,
            int64_col: 64,
            uint8_col: 8,
            uint16_col: 16,
            uint32_col: 32,
            uint64_col: 64,
            uint128_col: 128,
            uint256_col: u256::from(i256::from(256i128)),
            date_col: Date::from(NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()),
            datetime_col: DateTime::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(1_431_648_000, 0).unwrap(),
            )
            .unwrap(),
            datetime64_col: DateTime64::<3>::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(1_431_648_000, 0).unwrap(),
            )
            .unwrap(),
            #[cfg(feature = "rust_decimal")]
            decimal32_col: rust_decimal::Decimal::new(123_456, 4), // 12.3456 (6 digits, scale 4)
            #[cfg(not(feature = "rust_decimal"))]
            decimal32_col: FixedPoint32::<4>(123_456), // 12.3456
            #[cfg(feature = "rust_decimal")]
            decimal64_col: rust_decimal::Decimal::new(12_345_678, 6), /* 12.345678 (8 digits,
                                                                       * scale 6) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal64_col: FixedPoint64::<6>(12_345_678), // 12.345678
            #[cfg(feature = "rust_decimal")]
            decimal128_col: rust_decimal::Decimal::new(1_234_567_890, 8), /* 12.34567890 (10
                                                                           * digits, scale 8) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal128_col: FixedPoint128::<8>(1_234_567_890), // 12.34567890
            decimal256_col: FixedPoint256::<10>::from(12_345_678_901_i128), /* 12.345678901 (11
                                                                             * digits, scale 10) */
            nullable_string_col: Some("Test String 1".to_string()),
            nullable_int32_col: Some(42),
            nullable_uint64_col: Some(424_242),
            array_uint64_col: vec![1, 2, 3, 4],
            array_string_col: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            array_nullable_int32_col: vec![Some(1), Some(2), None],
            array_nullable_string_col: vec![Some("a".to_string()), Some("b".to_string()), None],
            string_col: "Regular String".to_string(),
            fixed_string_col: "Fixed".to_string(),
            uuid_col: Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap(),
        },
        // Row 2: Different values, some nullables are None
        TestRowAll {
            id: 2,
            int8_col: -8,
            int16_col: -16,
            int32_col: -32,
            int64_col: -64,
            uint8_col: 18,
            uint16_col: 1616,
            uint32_col: 323_232,
            uint64_col: 646_464,
            uint128_col: 128_128,
            uint256_col: u256::from(i256::from(512i128)),
            date_col: Date::from(NaiveDate::from_ymd_opt(2023, 2, 15).unwrap()),
            datetime_col: DateTime::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(1_582_911_293, 20).unwrap(),
            )
            .unwrap(),
            datetime64_col: DateTime64::<3>::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(1_582_911_293, 20).unwrap(),
            )
            .unwrap(),
            #[cfg(feature = "rust_decimal")]
            decimal32_col: rust_decimal::Decimal::new(-56789, 4), // -5.6789 (5 digits, scale 4)
            #[cfg(not(feature = "rust_decimal"))]
            decimal32_col: FixedPoint32::<4>(-56789), // -5.6789
            #[cfg(feature = "rust_decimal")]
            decimal64_col: rust_decimal::Decimal::new(-98_765_432, 6), /* -98.765432 (8 digits,
                                                                        * scale 6) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal64_col: FixedPoint64::<6>(-98_765_432), // -98.765432
            #[cfg(feature = "rust_decimal")]
            decimal128_col: rust_decimal::Decimal::new(-8_765_432_101, 8), /* -87.65432101 (10
                                                                            * digits, scale 8) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal128_col: FixedPoint128::<8>(-8_765_432_101), // -87.65432101
            decimal256_col: FixedPoint256::<10>::from(-98_765_432_101_i128), /* -98.765432101
                                                                              * (11
                                                                              * digits, scale
                                                                              * 10) */
            nullable_string_col: None,
            nullable_int32_col: Some(-99),
            nullable_uint64_col: None,
            array_uint64_col: vec![10, 20, 30],
            array_string_col: vec!["alpha".to_string(), "beta".to_string()],
            array_nullable_int32_col: vec![None],
            array_nullable_string_col: vec![None, Some("y".to_string()), None],
            string_col: "Another String Value".to_string(),
            fixed_string_col: "12345".to_string(),
            uuid_col: Uuid::parse_str("87654321-4321-8765-abcd-987654321000").unwrap(),
        },
        // Row 3: Edge cases and boundary values
        TestRowAll {
            id: 3,
            int8_col: i8::MAX,
            int16_col: i16::MAX,
            int32_col: i32::MAX,
            int64_col: i64::MAX,
            uint8_col: u8::MAX,
            uint16_col: u16::MAX,
            uint32_col: u32::MAX,
            uint64_col: u64::MAX,
            uint128_col: u128::MAX,
            uint256_col: u256([1; 32]),
            date_col: Date::from(NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()),
            datetime_col: DateTime::from_chrono_infallible_utc(
                chrono::DateTime::<chrono::Utc>::MAX_UTC,
            ),
            datetime64_col: DateTime64::<3>::try_from(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                .unwrap(),
            #[cfg(feature = "rust_decimal")]
            decimal32_col: rust_decimal::Decimal::new(99999, 4), // 9.9999 (5 digits, scale 4)
            #[cfg(not(feature = "rust_decimal"))]
            decimal32_col: FixedPoint32::<4>(99999), // 9.9999
            #[cfg(feature = "rust_decimal")]
            decimal64_col: rust_decimal::Decimal::new(99_999_999, 6), /* 99.999999 (8 digits,
                                                                       * scale 6) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal64_col: FixedPoint64::<6>(99_999_999), // 99.999999
            #[cfg(feature = "rust_decimal")]
            decimal128_col: rust_decimal::Decimal::new(9_999_999_999, 8), /* 99.99999999 (10
                                                                           * digits, scale 8) */
            #[cfg(not(feature = "rust_decimal"))]
            decimal128_col: FixedPoint128::<8>(9_999_999_999), // 99.99999999
            decimal256_col: FixedPoint256::<10>::from(99_999_999_999_i128), /* 99.999999999 (11
                                                                             * digits, scale 10) */
            nullable_string_col: Some(String::new()), // Empty string
            nullable_int32_col: Some(0),
            nullable_uint64_col: Some(0),
            array_uint64_col: vec![u64::MAX],
            array_string_col: vec![String::new()], // Empty string in array
            array_nullable_int32_col: vec![Some(i32::MAX)],
            array_nullable_string_col: vec![Some(String::new()), None, Some("z".to_string())],
            string_col: "特殊字符和Unicode测试".to_string(), // Unicode test
            fixed_string_col: "0".to_string(),
            uuid_col: Uuid::nil(), // Nil UUID
        },
        // Row 4: Minimal values
        TestRowAll {
            id: 4,
            int8_col: i8::MIN,
            int16_col: i16::MIN,
            int32_col: i32::MIN,
            int64_col: i64::MIN,
            uint8_col: 0,
            uint16_col: 0,
            uint32_col: 0,
            uint64_col: 0,
            uint128_col: 0,
            uint256_col: u256([0; 32]),
            date_col: Date::from(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
            datetime_col: DateTime::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            )
            .unwrap(),
            datetime64_col: DateTime64::<3>::try_from(
                chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            )
            .unwrap(),
            #[cfg(feature = "rust_decimal")]
            decimal32_col: rust_decimal::Decimal::ZERO, // 0 (1 digit, scale 0)
            #[cfg(not(feature = "rust_decimal"))]
            decimal32_col: FixedPoint32::<4>(0), // 0
            #[cfg(feature = "rust_decimal")]
            decimal64_col: rust_decimal::Decimal::ZERO, // 0 (1 digit, scale 0)
            #[cfg(not(feature = "rust_decimal"))]
            decimal64_col: FixedPoint64::<6>(0), // 0
            #[cfg(feature = "rust_decimal")]
            decimal128_col: rust_decimal::Decimal::ZERO, // 0 (1 digit, scale 0)
            #[cfg(not(feature = "rust_decimal"))]
            decimal128_col: FixedPoint128::<8>(0), // 0
            decimal256_col: FixedPoint256::<10>(i256([0; 32])), // 0
            nullable_string_col: Some("Just one more test".to_string()),
            nullable_int32_col: None,
            nullable_uint64_col: Some(u64::MAX),
            array_uint64_col: vec![], // Empty array
            array_string_col: vec!["only_one".to_string()],
            array_nullable_int32_col: vec![Some(i32::MIN)],
            array_nullable_string_col: vec![],
            string_col: "x".repeat(1000), // Long string
            fixed_string_col: "12".to_string(),
            uuid_col: Uuid::new_v4(), // Random UUID
        },
    ]
}

/// Test data for Variant type round trip
#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "derive", derive(Row))]
#[cfg_attr(feature = "derive", clickhouse_arrow(schema = get_variant_schema))]
pub struct TestRowVariant {
    id:                 u64,
    simple_variant:     Value, // Variant(String, UInt64)
    complex_variant:    Value, // Variant(Array(String), UUID, Tuple(String, UInt64))
    multi_type_variant: Value, // Variant(String, UInt64, Float64, Array(UInt8))
}

pub fn get_variant_schema() -> Vec<ColumnDefinition> {
    vec![
        ("id".to_string(), Type::UInt64, None),
        ("simple_variant".to_string(), Type::Variant(vec![Type::String, Type::UInt64]), None),
        (
            "complex_variant".to_string(),
            Type::Variant(vec![
                Type::Array(Box::new(Type::String)),
                Type::Uuid,
                Type::Tuple(vec![Type::String, Type::UInt64]),
            ]),
            None,
        ),
        (
            "multi_type_variant".to_string(),
            Type::Variant(vec![
                Type::String,
                Type::UInt64,
                Type::Float64,
                Type::Array(Box::new(Type::UInt8)),
            ]),
            None,
        ),
    ]
}

pub fn generate_variant_test_block() -> Vec<TestRowVariant> {
    vec![
        // Test different variant types
        TestRowVariant {
            id:                 1,
            simple_variant:     Value::Variant(0, Box::new(Value::String(b"hello".to_vec()))), /* String (discriminator 0) */
            complex_variant:    Value::Variant(
                0,
                Box::new(Value::Array(vec![
                    Value::String(b"a".to_vec()),
                    Value::String(b"b".to_vec()),
                ])),
            ), /* Array(String) (discriminator 0) */
            multi_type_variant: Value::Variant(1, Box::new(Value::Float64(3.14))), /* Float64 (discriminator 1) */
        },
        TestRowVariant {
            id:                 2,
            simple_variant:     Value::Variant(1, Box::new(Value::UInt64(123))), // UInt64
            complex_variant:    Value::Variant(2, Box::new(Value::Uuid(Uuid::new_v4()))), /* UUID (discriminator 2) */
            multi_type_variant: Value::Variant(
                0,
                Box::new(Value::Array(vec![Value::UInt8(1), Value::UInt8(2), Value::UInt8(3)])),
            ), /* Array(UInt8) */
        },
        TestRowVariant {
            id:                 3,
            simple_variant:     Value::Variant(0, Box::new(Value::String(b"world".to_vec()))), /* String */
            complex_variant:    Value::Variant(
                1,
                Box::new(Value::Tuple(vec![Value::String(b"test".to_vec()), Value::UInt64(42)])),
            ), /* Tuple (discriminator 1) */
            multi_type_variant: Value::Variant(2, Box::new(Value::String(b"test".to_vec()))), /* String */
        },
        // Test NULL variant values
        TestRowVariant {
            id:                 4,
            simple_variant:     Value::Variant(0xFF, Box::new(Value::Null)), // NULL variant
            complex_variant:    Value::Variant(0xFF, Box::new(Value::Null)), // NULL variant
            multi_type_variant: Value::Variant(3, Box::new(Value::UInt64(999))), // UInt64
        },
    ]
}

pub fn generate_dynamic_test_block() -> Block {
    let rows = vec![
        // Test simple types first for debugging
        Value::Int32(42),
        Value::String(b"hello".to_vec()),
        Value::Float64(3.141_592_653_589_793),
    ];

    Block {
        info:         Default::default(),
        rows:         rows.len() as u64,
        column_types: vec![("dynamic_col".to_string(), Type::Dynamic)],
        column_data:  rows,
    }
}

pub fn generate_json_test_block() -> Block {
    let rows = vec![
        // Test simple types first for debugging
        Value::Int32(42),
        Value::String(b"hello".to_vec()),
        Value::Float64(3.141_592_653_589_793),
    ];

    Block {
        info:         Default::default(),
        rows:         rows.len() as u64,
        column_types: vec![("json_col".to_string(), Type::JSON)],
        column_data:  rows,
    }
}
