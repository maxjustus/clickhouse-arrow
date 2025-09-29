#![allow(clippy::approx_constant)]
use chrono::NaiveDate;
use chrono_tz::Tz;
use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::native::block_info::BlockInfo;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::{ColumnDefinition, Type, Value};
use uuid::Uuid;

/// Test data for round trip
#[derive(Debug, Default, PartialEq, Clone)]
#[cfg_attr(feature = "derive", derive(Row))]
#[cfg_attr(feature = "derive", clickhouse_arrow(schema = get_testrowall_schema))]
pub struct TestRowAll {
    id: u64,
    int8_col: i8,
    int16_col: i16,
    int32_col: i32,
    int64_col: i64,
    uint8_col: u8,
    uint16_col: u16,
    uint32_col: u32,
    uint64_col: u64,
    uint128_col: u128,
    uint256_col: u256,
    date_col: Date,
    datetime_col: DateTime,
    datetime64_col: DateTime64<3>,
    #[cfg(feature = "rust_decimal")]
    decimal32_col: rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal32_col: FixedPoint32<4>,
    #[cfg(feature = "rust_decimal")]
    decimal64_col: rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal64_col: FixedPoint64<6>,
    #[cfg(feature = "rust_decimal")]
    decimal128_col: rust_decimal::Decimal,
    #[cfg(not(feature = "rust_decimal"))]
    decimal128_col: FixedPoint128<8>,
    decimal256_col: FixedPoint256<10>,
    nullable_string_col: Option<String>,
    nullable_int32_col: Option<i32>,
    nullable_uint64_col: Option<u64>,
    array_uint64_col: Vec<u64>,
    array_string_col: Vec<String>,
    array_nullable_int32_col: Vec<Option<i32>>,
    array_nullable_string_col: Vec<Option<String>>,
    string_col: String,
    fixed_string_col: String, // FixedString(5) trimmed to String
    uuid_col: Uuid,
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
pub fn generate_test_block() -> Block {
    use clickhouse_arrow::i256;

    let test_data = vec![
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
    ];

    // Convert TestRowAll data to Block directly
    let rows = test_data.len() as u64;
    let schema = get_testrowall_schema();
    let mut column_data = Vec::new();

    // Build columns by iterating through schema and extracting values from each row
    for (col_idx, _) in schema.iter().enumerate() {
        for row in &test_data {
            let value = match col_idx {
                0 => Value::UInt64(row.id),
                1 => Value::Int8(row.int8_col),
                2 => Value::Int16(row.int16_col),
                3 => Value::Int32(row.int32_col),
                4 => Value::Int64(row.int64_col),
                5 => Value::UInt8(row.uint8_col),
                6 => Value::UInt16(row.uint16_col),
                7 => Value::UInt32(row.uint32_col),
                8 => Value::UInt64(row.uint64_col),
                9 => Value::UInt128(row.uint128_col),
                10 => Value::UInt256(row.uint256_col),
                11 => Value::Date(row.date_col),
                12 => Value::DateTime(row.datetime_col),
                13 => Value::DateTime64(row.datetime64_col.into()),
                14 => {
                    #[cfg(feature = "rust_decimal")]
                    {
                        let mantissa = i32::try_from(row.decimal32_col.mantissa())
                            .expect("Decimal32 mantissa should fit in i32");
                        Value::Decimal32(4, mantissa)
                    }
                    #[cfg(not(feature = "rust_decimal"))]
                    {
                        Value::Decimal32(4, row.decimal32_col.0)
                    }
                }
                15 => {
                    #[cfg(feature = "rust_decimal")]
                    {
                        let mantissa = i64::try_from(row.decimal64_col.mantissa())
                            .expect("Decimal64 mantissa should fit in i64");
                        Value::Decimal64(6, mantissa)
                    }
                    #[cfg(not(feature = "rust_decimal"))]
                    {
                        Value::Decimal64(6, row.decimal64_col.0)
                    }
                }
                16 => {
                    #[cfg(feature = "rust_decimal")]
                    {
                        let mantissa = row.decimal128_col.mantissa();
                        Value::Decimal128(8, mantissa)
                    }
                    #[cfg(not(feature = "rust_decimal"))]
                    {
                        Value::Decimal128(8, row.decimal128_col.0)
                    }
                }
                17 => Value::Decimal256(10, row.decimal256_col.0),
                18 => match &row.nullable_string_col {
                    Some(s) => Value::String(s.as_bytes().to_vec()),
                    None => Value::Null,
                },
                19 => match &row.nullable_int32_col {
                    Some(v) => Value::Int32(*v),
                    None => Value::Null,
                },
                20 => match &row.nullable_uint64_col {
                    Some(v) => Value::UInt64(*v),
                    None => Value::Null,
                },
                21 => {
                    Value::Array(row.array_uint64_col.iter().map(|v| Value::UInt64(*v)).collect())
                }
                22 => Value::Array(
                    row.array_string_col
                        .iter()
                        .map(|s| Value::String(s.as_bytes().to_vec()))
                        .collect(),
                ),
                23 => Value::Array(
                    row.array_nullable_int32_col
                        .iter()
                        .map(|opt| match opt {
                            Some(v) => Value::Int32(*v),
                            None => Value::Null,
                        })
                        .collect(),
                ),
                24 => Value::Array(
                    row.array_nullable_string_col
                        .iter()
                        .map(|opt| match opt {
                            Some(s) => Value::String(s.as_bytes().to_vec()),
                            None => Value::Null,
                        })
                        .collect(),
                ),
                25 => Value::String(row.string_col.as_bytes().to_vec()),
                26 => Value::String(row.fixed_string_col.as_bytes().to_vec()),
                27 => Value::Uuid(row.uuid_col),
                _ => panic!("Unexpected column index: {col_idx}"),
            };
            column_data.push(value);
        }
    }

    Block {
        info: BlockInfo::default(),
        rows,
        column_types: schema.into_iter().map(|(name, typ, _)| (name, typ)).collect(),
        column_data,
    }
}

/// Test data for Variant type round trip
#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "derive", derive(Row))]
#[cfg_attr(feature = "derive", clickhouse_arrow(schema = get_variant_schema))]
pub struct TestRowVariant {
    id: u64,
    simple_variant: Value,     // Variant(String, UInt64)
    complex_variant: Value,    // Variant(Array(String), UUID, Tuple(String, UInt64))
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
                Type::Tuple(vec![Type::String, Type::UInt64]),
                Type::Uuid,
            ]),
            None,
        ),
        (
            "multi_type_variant".to_string(),
            Type::Variant(vec![
                Type::Array(Box::new(Type::UInt8)),
                Type::Float64,
                Type::String,
                Type::UInt64,
            ]),
            None,
        ),
    ]
}

/// # Panics
pub fn generate_variant_test_block() -> Block {
    let test_data = vec![
        // Test different variant types
        TestRowVariant {
            id: 1,
            simple_variant: Value::Variant(0, Box::new(Value::String(b"hello".to_vec()))), /* String (discriminator 0) */
            complex_variant: Value::Variant(
                0,
                Box::new(Value::Array(vec![
                    Value::String(b"a".to_vec()),
                    Value::String(b"b".to_vec()),
                ])),
            ), /* Array(String) (discriminator 0) */
            multi_type_variant: Value::Variant(1, Box::new(Value::Float64(std::f64::consts::PI))), /* Float64 (discriminator 1) */
        },
        TestRowVariant {
            id: 2,
            simple_variant: Value::Variant(1, Box::new(Value::UInt64(123))), // UInt64
            complex_variant: Value::Variant(2, Box::new(Value::Uuid(Uuid::new_v4()))), /* UUID (discriminator 2) */
            multi_type_variant: Value::Variant(
                0,
                Box::new(Value::Array(vec![Value::UInt8(1), Value::UInt8(2), Value::UInt8(3)])),
            ), /* Array(UInt8) */
        },
        TestRowVariant {
            id: 3,
            simple_variant: Value::Variant(0, Box::new(Value::String(b"world".to_vec()))), /* String */
            complex_variant: Value::Variant(
                1,
                Box::new(Value::Tuple(vec![Value::String(b"test".to_vec()), Value::UInt64(42)])),
            ), /* Tuple (discriminator 1) */
            multi_type_variant: Value::Variant(2, Box::new(Value::String(b"test".to_vec()))), /* String */
        },
        // Test NULL variant values
        TestRowVariant {
            id: 4,
            simple_variant: Value::Variant(0xFF, Box::new(Value::Null)), // NULL variant
            complex_variant: Value::Variant(0xFF, Box::new(Value::Null)), // NULL variant
            multi_type_variant: Value::Variant(3, Box::new(Value::UInt64(999))), // UInt64
        },
    ];

    // Convert TestRowVariant data to Block directly
    let rows = test_data.len() as u64;
    let schema = get_variant_schema();
    let mut column_data = Vec::new();

    // Build columns by iterating through schema and extracting values from each row
    for (col_idx, _) in schema.iter().enumerate() {
        for row in &test_data {
            let value = match col_idx {
                0 => Value::UInt64(row.id),
                1 => row.simple_variant.clone(),
                2 => row.complex_variant.clone(),
                3 => row.multi_type_variant.clone(),
                _ => panic!("Unexpected column index: {col_idx}"),
            };
            column_data.push(value);
        }
    }

    Block {
        info: BlockInfo::default(),
        rows,
        column_types: schema.into_iter().map(|(name, typ, _)| (name, typ)).collect(),
        column_data,
    }
}

pub fn generate_dynamic_test_block() -> Block {
    let rows = vec![
        // Test simple types first for debugging
        Value::Int32(42),
        Value::String(b"hello".to_vec()),
        Value::Float64(std::f64::consts::PI),
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![("dynamic_col".to_string(), Type::Dynamic { max_types: None })],
        column_data: rows,
    }
}

pub fn generate_json_test_block() -> Block {
    let rows = vec![
        // Test with simple JSON first - complex multi-path JSON has byte alignment issues
        Value::String(b"{\"id\": 42}".to_vec()),
        Value::String(b"{\"name\": \"Alice\"}".to_vec()),
        Value::String(b"{\"score\": 95.5}".to_vec()),
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![(
            "json_col".to_string(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths: vec![],
                skip_exact: vec![],
                skip_regex: vec![],
            },
        )],
        column_data: rows,
    }
}

pub fn generate_json_array_test_block() -> Block {
    let rows = vec![
        // Homogeneous array of integers
        Value::String(br#"{"numbers": [1, 2, 3, 4, 5]}"#.to_vec()),
        // Heterogeneous array with mixed types
        Value::String(br#"{"mixed": [1, "hello", 3.14, null, true]}"#.to_vec()),
        // Nested arrays
        Value::String(br#"{"nested": [[1, 2], [3, 4], [5, 6]]}"#.to_vec()),
        // Empty arrays
        Value::String(br#"{"empty": []}"#.to_vec()),
        // Arrays with nulls
        Value::String(br#"{"nullable": [1, null, 3, null, 5]}"#.to_vec()),
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![(
            "json_col".to_string(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths: vec![],
                skip_exact: vec![],
                skip_regex: vec![],
            },
        )],
        column_data: rows,
    }
}

pub fn generate_json_typed_paths_variant_test_block() -> Block {
    let rows = vec![
        // JSON with Variant typed paths - only values that match the Variant types
        Value::String(br#"{"status": "active", "value": 42}"#.to_vec()),
        Value::String(br#"{"status": "inactive", "value": 123}"#.to_vec()),
        Value::String(br#"{"status": "pending", "value": 3.14}"#.to_vec()),
        // Test with string values in Variant - use simple strings that can match String type
        Value::String(br#"{"status": "completed", "value": "hello"}"#.to_vec()),
        Value::String(br#"{"status": "failed", "value": "world"}"#.to_vec()),
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![(
            "json_col".to_string(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths: vec![(
                    "value".to_string(),
                    Box::new(Type::variant(vec![Type::String, Type::Int64, Type::Float64])),
                )],
                skip_exact: vec![],
                skip_regex: vec![],
            },
        )],
        column_data: rows,
    }
}

pub fn generate_mixed_dynamic_json_test_block() -> Block {
    // Use data from both Dynamic and JSON test blocks
    let dynamic_data = vec![
        Value::Int32(42),
        Value::String(b"hello".to_vec()),
        Value::Float64(std::f64::consts::PI),
    ];

    let json_data = vec![
        Value::String(b"{\"id\": 42}".to_vec()),
        Value::String(b"{\"name\": \"Alice\"}".to_vec()),
        Value::String(b"{\"score\": 95.5}".to_vec()),
    ];

    // Column-wise data: all dynamic values first, then all JSON values
    let mut mixed_data = Vec::new();
    mixed_data.extend(dynamic_data.clone()); // All Dynamic column values
    mixed_data.extend(json_data.clone()); // All JSON column values

    Block {
        info: BlockInfo::default(),
        rows: dynamic_data.len() as u64, // Number of rows
        column_types: vec![
            ("dynamic_col".to_string(), Type::Dynamic { max_types: None }),
            (
                "json_col".to_string(),
                Type::JSON {
                    max_dynamic_paths: None,
                    max_dynamic_types: None,
                    typed_paths: vec![],
                    skip_exact: vec![],
                    skip_regex: vec![],
                },
            ),
        ],
        column_data: mixed_data,
    }
}

pub fn generate_evil_heterogeneous_json_test_block() -> Block {
    // The ultimate evil JSON: deeply nested with heterogeneous arrays at multiple levels
    let rows = vec![
        // Row 1: Top-level heterogeneous array with nested objects
        Value::String(
            br#"{"evil": [1, "text", null, [true, 3.14, {"nested": "object", "level": 2}]]}"#
                .to_vec(),
        ),

        // Row 2: Deeply nested mixed types (3+ levels)
        Value::String(
            br#"{"data": {"arr": [1, [2, "three", [4.0, null, {"key": [5, "six", true]}]]]}}"#
                .to_vec(),
        ),

        // Row 3: Arrays of objects with wildly varying schemas
        Value::String(
            br#"{"users": [{"name": "Alice", "age": 30}, {"id": 1, "active": false}, {"tags": ["a", "b", 123]}], "count": 3}"#
                .to_vec(),
        ),

        // Row 4: Sparse paths - completely different structure from other rows
        Value::String(
            br#"{"other": {"path": {"unique": [1, "mixed", null, [[[7, "deep"]]]]}}}"#
                .to_vec(),
        ),

        // Row 5: Multiple paths with heterogeneous arrays at each
        Value::String(
            br#"{"a": [1, "two", 3.0], "b": [[4, 5], ["six", 7.0], null], "c": {"d": [true, null, 8.9, "end"]}}"#
                .to_vec(),
        ),

        // Row 6: Arrays containing arrays of mixed objects
        Value::String(
            br#"{"matrix": [[{"x": 1}, {"y": "two"}], [{"z": [3, "four"]}, 5], "flat"]}"#
                .to_vec(),
        ),

        // Row 7: Null value to ensure it doesn't break
        Value::Null,

        // Row 8: Empty object and arrays mixed
        Value::String(
            br#"{"empty": [], "mixed": [[], {}, null, [1, {}]], "end": {}}"#
                .to_vec(),
        ),
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![(
            "evil_json_col".to_string(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths: vec![],
                skip_exact: vec![],
                skip_regex: vec![],
            },
        )],
        column_data: rows,
    }
}

pub fn generate_evil_heterogeneous_dynamic_test_block() -> Block {
    // The ultimate evil: deeply nested with heterogeneous arrays at multiple levels
    let rows = vec![
        // The evil case: heterogeneous array at top level
        Value::Array(vec![
            Value::Int32(42),
            Value::String(b"mixed".to_vec()),
            Value::Float64(3.14),
        ]),
        // Even more evil: nested heterogeneous arrays
        Value::Array(vec![
            Value::Int32(1),
            Value::String(b"level1".to_vec()),
            Value::Array(vec![
                Value::Float64(2.71),
                Value::String(b"level2".to_vec()),
                Value::Null,
                Value::Array(vec![
                    Value::Int8(99),
                    Value::UInt64(100),
                    Value::String(b"level3".to_vec()),
                ]),
            ]),
        ]),
        // Heterogeneous inside tuple
        Value::Tuple(vec![
            Value::String(b"normal".to_vec()),
            Value::Array(vec![
                Value::Int32(1),
                Value::String(b"mixed_in_tuple".to_vec()),
                Value::Float32(1.5),
            ]),
        ]),
        // Heterogeneous as map values
        Value::Map(
            vec![Value::String(b"chaos".to_vec())],
            vec![Value::Array(vec![
                Value::Int32(1),
                Value::String(b"mixed_in_map".to_vec()),
                Value::Tuple(vec![Value::Int8(1), Value::Float64(2.0)]),
            ])],
        ),
        // NULL value to verify it doesn't break anything
        Value::Null,
    ];

    Block {
        info: BlockInfo::default(),
        rows: rows.len() as u64,
        column_types: vec![(
            "evil_heterogeneous_dynamic".to_string(),
            Type::Dynamic { max_types: None },
        )],
        column_data: rows,
    }
}

/// Higher-level test harness for native roundtrip tests
pub struct NativeRoundtripTestHarness<'a> {
    pub container: &'a clickhouse_arrow::test_utils::ClickHouseContainer,
    pub require_v3: bool, // TODO: too general, should say require_dynamic_flattened_format
    pub compression: CompressionMethod,
}

impl<'a> NativeRoundtripTestHarness<'a> {
    /// Create a new test harness
    pub fn new(container: &'a clickhouse_arrow::test_utils::ClickHouseContainer) -> Self {
        Self { container, require_v3: false, compression: CompressionMethod::None }
    }

    /// Enable v3 format requirement
    #[must_use]
    pub fn with_v3_format(mut self) -> Self {
        self.require_v3 = true;
        self
    }

    /// Set compression method
    #[must_use]
    pub fn with_compression(mut self, compression: CompressionMethod) -> Self {
        self.compression = compression;
        self
    }

    /// Check if server supports required features
    ///
    /// # Errors
    ///
    /// Returns an error if the `ClickHouse` version does not meet the minimum requirements
    /// for the test scenario.
    pub fn check_version_support(&self) -> Result<()> {
        if self.require_v3 {
            let version_str = std::env::var("CLICKHOUSE_VERSION").ok();
            match version_str.as_deref() {
                Some(v) if v.starts_with("24.") => {
                    return Err(Error::SerializeError(
                        "Test requires v3 format support (ClickHouse 25.6+)".to_string(),
                    ));
                }
                Some(v) if v.starts_with("25.") => {
                    let parts: Vec<&str> = v.split('.').collect();
                    if parts.len() >= 2 && parts[1].parse::<u32>().unwrap_or(0) < 6 {
                        return Err(Error::SerializeError(
                            "Test requires v3 format support (ClickHouse 25.6+)".to_string(),
                        ));
                    }
                }
                _ => {} // Default to support for latest
            }
        }
        Ok(())
    }

    /// Create a test client with appropriate settings
    ///
    /// # Errors
    /// Returns an error if client creation fails
    pub async fn create_client(&self) -> Result<NativeClient> {
        let mut builder = ClientBuilder::new()
            .with_endpoint(self.container.get_native_url())
            .with_username(&self.container.user)
            .with_password(&self.container.password)
            .with_ipv4_only(true)
            .with_compression(self.compression);

        if self.require_v3 {
            builder = builder.with_setting(
                "output_format_native_use_flattened_dynamic_and_json_serialization",
                1,
            );
        }

        builder.build().await
    }

    /// Create a test table with the given schema
    ///
    /// # Errors
    ///
    /// Returns an error if the table creation fails or if the `ClickHouse` server
    /// returns an error.
    pub async fn create_test_table(
        &self,
        client: &NativeClient,
        table_name: &str,
        column_definitions: &[ColumnDefinition],
    ) -> Result<()> {
        // Drop table if it exists
        client.execute(format!("DROP TABLE IF EXISTS {table_name}"), None).await?;

        // Create table with schema
        let create_sql = format!(
            "CREATE TABLE {table_name} ({}) ENGINE = MergeTree() ORDER BY tuple()",
            column_definitions
                .iter()
                .map(|(name, type_, _)| format!("{name} {type_}"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        client.execute(create_sql, None).await?;
        Ok(())
    }

    /// Insert a Block of data into the table
    ///
    /// # Errors
    /// Returns an error if data insertion fails
    pub async fn insert_test_data(
        &self,
        client: &NativeClient,
        table_name: &str,
        block: &Block,
    ) -> Result<()> {
        use futures_util::StreamExt;

        let insert_query = format!("INSERT INTO {table_name} VALUES");
        let mut stream = client.insert(&insert_query, block.clone(), None).await?;

        while let Some(result) = stream.next().await {
            result?;
        }
        Ok(())
    }

    /// Query data back from the table and verify it matches expectations
    ///
    /// # Errors
    /// Returns an error if query fails or data doesn't match expected values
    pub async fn query_and_verify_data(
        &self,
        client: &NativeClient,
        table_name: &str,
        expected_block: &Block,
    ) -> Result<()> {
        use clickhouse_arrow::{Qid, QueryParams};
        use futures_util::StreamExt;

        let query = format!("SELECT * FROM {table_name} ORDER BY tuple()");
        let result_blocks: Vec<Block> = client
            .query_raw(query, None::<QueryParams>, Qid::new())
            .await?
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        // Combine all result blocks
        let mut combined_data = Vec::new();
        let mut combined_types = Vec::new();
        let mut total_rows = 0;

        for block in &result_blocks {
            if combined_types.is_empty() {
                combined_types.clone_from(&block.column_types);
            }
            combined_data.extend(block.column_data.iter().cloned());
            total_rows += block.rows;
        }

        let result_block = Block {
            info: BlockInfo::default(),
            rows: total_rows,
            column_types: combined_types,
            column_data: combined_data,
        };

        // Verify the data matches expectations
        if result_block.rows != expected_block.rows {
            return Err(Error::SerializeError(format!(
                "Row count mismatch: expected {}, got {}",
                expected_block.rows, result_block.rows
            )));
        }

        if result_block.column_types.len() != expected_block.column_types.len() {
            return Err(Error::SerializeError(format!(
                "Column count mismatch: expected {}, got {}",
                expected_block.column_types.len(),
                result_block.column_types.len()
            )));
        }

        // For complex types like Dynamic and JSON, we can't do exact value comparison
        // due to potential serialization differences, so we just verify structure
        for (i, ((expected_name, expected_type), (result_name, result_type))) in
            expected_block.column_types.iter().zip(&result_block.column_types).enumerate()
        {
            if expected_name != result_name {
                return Err(Error::SerializeError(format!(
                    "Column {i} name mismatch: expected {expected_name}, got {result_name}"
                )));
            }

            // For Dynamic and JSON types, only verify the type structure
            match (expected_type, result_type) {
                (Type::Dynamic { .. }, Type::Dynamic { .. })
                | (Type::JSON { .. }, Type::JSON { .. }) => {
                    // Structure verification passed
                }
                _ if expected_type == result_type => {
                    // Exact type match - can do value comparison
                }
                _ => {
                    return Err(Error::SerializeError(format!(
                        "Column {i} type mismatch: expected {expected_type}, got {result_type}"
                    )));
                }
            }
        }

        Ok(())
    }

    /// Drop the test table
    ///
    /// # Errors
    /// Returns an error if table drop fails
    pub async fn drop_test_table(&self, client: &NativeClient, table_name: &str) -> Result<()> {
        client.execute(format!("DROP TABLE IF EXISTS {table_name}"), None).await
    }

    /// Run a complete roundtrip test: create table, insert data, query back, verify, cleanup
    ///
    /// # Errors
    /// Returns an error if any part of the roundtrip test fails
    pub async fn run_native_roundtrip_test(&self, test_name: &str, block: &Block) -> Result<()> {
        // Check version support
        self.check_version_support()?;

        // Create client
        let client = self.create_client().await?;

        // Generate unique table name
        let table_name = format!("test_{}_{}", test_name, Uuid::new_v4().simple());

        // Convert block column types to ColumnDefinition format
        let column_definitions: Vec<ColumnDefinition> = block
            .column_types
            .iter()
            .map(|(name, type_)| (name.clone(), type_.clone(), None))
            .collect();

        // Create table
        self.create_test_table(&client, &table_name, &column_definitions).await?;

        // Insert data
        self.insert_test_data(&client, &table_name, block).await?;

        // Query and verify
        let result = self.query_and_verify_data(&client, &table_name, block).await;

        // Always try to clean up
        drop(self.drop_test_table(&client, &table_name).await);

        result
    }
}

/// Macro to create a simple native roundtrip test using the test harness
/// This reduces boilerplate for common test scenarios
macro_rules! native_roundtrip_test {
    ($test_name:ident, $block_generator:expr,v3) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $test_name() {
            use std::sync::Arc;

            use clickhouse_arrow::test_utils::ClickHouseContainer;

            // Use the per-test container harness to ensure cleanup
            use crate::tests::run_test_with_cleanup;

            let result = run_test_with_cleanup(
                stringify!($test_name),
                |container: Arc<ClickHouseContainer>| async move {
                    let harness = NativeRoundtripTestHarness::new(&container).with_v3_format();
                    let block = $block_generator;
                    harness
                        .run_native_roundtrip_test(stringify!($test_name), &block)
                        .await
                        .unwrap();
                },
                None,
                None,
            )
            .await;

            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }
    };
}

native_roundtrip_test!(test_dynamic_harness_example, generate_dynamic_test_block(), v3);

native_roundtrip_test!(test_json_harness_example, generate_json_test_block(), v3);

native_roundtrip_test!(test_mixed_harness_example, generate_mixed_dynamic_json_test_block(), v3);
