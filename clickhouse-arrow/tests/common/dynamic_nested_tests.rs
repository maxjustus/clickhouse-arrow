use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::native::block_info::BlockInfo;
use clickhouse_arrow::prelude::*;

/// Generate a test block with nested types for Dynamic columns
pub fn generate_nested_dynamic_test_block() -> Block {
    let rows = vec![
        // Simple types (baseline)
        Value::Int32(42),
        Value::String(b"hello".to_vec()),
        // Homogeneous nested Array
        Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]),
        // Homogeneous Array of Arrays
        Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]),
        // Tuple with mixed types (but each position has consistent type)
        Value::Tuple(vec![Value::String(b"name".to_vec()), Value::Int32(25), Value::Float64(3.14)]),
        // Map with homogeneous nested values
        Value::Map(vec![Value::String(b"key1".to_vec()), Value::String(b"key2".to_vec())], vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]),
        // Complex nested but still homogeneous: Array of Tuples
        Value::Array(vec![
            Value::Tuple(vec![Value::String(b"item1".to_vec()), Value::Float64(19.99)]),
            Value::Tuple(vec![Value::String(b"item2".to_vec()), Value::Float64(29.99)]),
        ]),
        // NULL value
        Value::Null,
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("nested_dynamic".to_string(), Type::Dynamic { max_types: None })],
        column_data:  rows,
    }
}

/// Generate test cases for heterogeneous arrays (currently unsupported)
pub fn generate_heterogeneous_dynamic_test_block() -> Block {
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
        Value::Map(vec![Value::String(b"chaos".to_vec())], vec![Value::Array(vec![
            Value::Int32(1),
            Value::String(b"mixed_in_map".to_vec()),
            Value::Tuple(vec![Value::Int8(1), Value::Float64(2.0)]),
        ])]),
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("heterogeneous_dynamic".to_string(), Type::Dynamic {
            max_types: None,
        })],
        column_data:  rows,
    }
}

/// Generate test cases for `max_types` validation with nested types
pub fn generate_max_types_test_block() -> Block {
    // This should trigger max_types validation when max_types is set low
    let rows = vec![
        Value::Int32(1),                                          // Type 1
        Value::String(b"test".to_vec()),                          // Type 2
        Value::Float64(3.14),                                     // Type 3
        Value::Array(vec![Value::Int32(1)]),                      // Type 4: Array(Int32)
        Value::Array(vec![Value::String(b"x".to_vec())]),         // Type 5: Array(String)
        Value::Tuple(vec![Value::Int32(1), Value::Float64(2.0)]), // Type 6: Tuple(Int32, Float64)
        Value::Map(
            // Type 7: Map(String, Int32)
            vec![Value::String(b"k".to_vec())],
            vec![Value::Int32(1)],
        ),
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("limited_dynamic".to_string(), Type::Dynamic { max_types: Some(5) })],
        column_data:  rows,
    }
}

// Note: These tests need to be in the main library code to access private APIs
// They are kept here as documentation of what should be tested
#[cfg(test)]
mod tests {
    use super::*;
    // These would need access to private modules:
    // use clickhouse_arrow::native::types::serialize::dynamic::DynamicSerializer;
    // use clickhouse_arrow::formats::TypeSpecificState;

    #[test]
    fn test_homogeneous_nested_type_detection() {
        use clickhouse_arrow::formats::TypeSpecificState;
        use clickhouse_arrow::native::types::serialize::dynamic::DynamicSerializer;

        let block = generate_nested_dynamic_test_block();

        // Analyze the values to build type registry
        let state = DynamicSerializer::analyze_values(&block.column_data);

        if let TypeSpecificState::Dynamic(dynamic_state) = state {
            let type_names = &dynamic_state.type_names;

            // Verify we detected all unique types
            assert!(type_names.contains(&"Int32".to_string()));
            assert!(type_names.contains(&"String".to_string()));
            assert!(type_names.contains(&"Array(Int32)".to_string()));
            assert!(type_names.contains(&"Array(Array(Int32))".to_string()));
            assert!(type_names.contains(&"Tuple(String, Int32, Float64)".to_string()));
            assert!(type_names.contains(&"Map(String, Array(Int32))".to_string()));

            // Complex nested types
            assert!(type_names.iter().any(|t| t.contains("Array(Tuple(String, Float64))")));

            println!("Detected {} unique types:", type_names.len());
            for (i, type_name) in type_names.iter().enumerate() {
                println!("  {i}: {type_name}");
            }
        } else {
            panic!("Expected Dynamic state");
        }
    }

    #[test]
    fn test_recursive_guess_type_homogeneous() {
        // Test deeply nested but homogeneous structure
        let nested = Value::Array(vec![
            Value::Tuple(vec![
                Value::Int32(1),
                Value::Map(vec![Value::String(b"key".to_vec())], vec![Value::Array(vec![
                    Value::Float64(3.14),
                    Value::Float64(2.71),
                ])]),
            ]),
            Value::Tuple(vec![
                Value::Int32(2),
                Value::Map(vec![Value::String(b"key2".to_vec())], vec![Value::Array(vec![
                    Value::Float64(1.41),
                    Value::Float64(1.73),
                ])]),
            ]),
        ]);

        let guessed = nested.guess_type();
        let type_string = guessed.to_string();

        // Verify the complete nested type was detected
        assert_eq!(type_string, "Array(Tuple(Int32, Map(String, Array(Float64))))");
    }

    #[test]
    fn test_heterogeneous_array_detection() {
        use clickhouse_arrow::formats::TypeSpecificState;
        use clickhouse_arrow::native::types::serialize::dynamic::DynamicSerializer;

        let block = generate_heterogeneous_dynamic_test_block();

        let state = DynamicSerializer::analyze_values(&block.column_data);

        if let TypeSpecificState::Dynamic(dynamic_state) = state {
            let type_names = &dynamic_state.type_names;

            // When implemented, should detect Variant-wrapped types
            assert!(type_names.iter().any(|t| t.contains("Array(Variant")));

            println!("Heterogeneous types detected:");
            for type_name in type_names {
                println!("  {type_name}");
            }
        } else {
            panic!("Expected Dynamic state");
        }
    }

    #[test]
    fn test_evil_deeply_nested_heterogeneous() {
        // The ultimate evil: deeply nested with heterogeneous arrays at multiple levels
        let evil = Value::Array(vec![
            Value::Int32(42),
            Value::String(b"chaos".to_vec()),
            Value::Array(vec![
                Value::Float64(3.14),
                Value::Null,
                Value::Array(vec![
                    Value::Int8(1),
                    Value::String(b"deep".to_vec()),
                    Value::Tuple(vec![
                        Value::Float32(1.5),
                        Value::Array(vec![Value::UInt64(999), Value::String(b"deepest".to_vec())]),
                    ]),
                ]),
            ]),
        ]);

        // When working, this should produce something like:
        // Array(Variant(Int32, String, Array(Variant(Float64, Null, Array(Variant(...))))))
        let guessed = evil.guess_type();
        let type_string = guessed.to_string();

        println!("Evil nested type: {type_string}");
        assert!(
            type_string.contains("Variant"),
            "Should detect and wrap heterogeneous arrays in Variant"
        );
    }

    #[test]
    fn test_current_heterogeneous_limitation() {
        // This test verifies that heterogeneous arrays are correctly detected
        // and wrapped in Variant types

        let mixed_array = Value::Array(vec![
            Value::Int32(1),
            Value::String(b"oops".to_vec()),
            Value::Float64(3.14),
        ]);

        let guessed = mixed_array.guess_type();

        // Correct behavior: detects all element types and wraps in Variant
        assert_eq!(guessed.to_string(), "Array(Variant(Float64, Int32, String))");
        // Types are sorted alphabetically in the Variant
    }

    #[test]
    fn test_type_registry_sorting() {
        use clickhouse_arrow::formats::TypeSpecificState;
        use clickhouse_arrow::native::types::serialize::dynamic::DynamicSerializer;

        let rows = vec![
            Value::String(b"z".to_vec()),
            Value::Int32(1),
            Value::Array(vec![Value::Int32(1)]),
            Value::Float64(3.14),
        ];

        let state = DynamicSerializer::analyze_values(&rows);

        if let TypeSpecificState::Dynamic(dynamic_state) = state {
            let type_names = &dynamic_state.type_names;

            // Verify alphabetical ordering
            assert_eq!(type_names[0], "Array(Int32)");
            assert_eq!(type_names[1], "Float64");
            assert_eq!(type_names[2], "Int32");
            assert_eq!(type_names[3], "String");
        }
    }

    // Note: max_types test would need the refactored version that takes Type as parameter
    // Currently analyze_values doesn't have access to the max_types constraint
}
