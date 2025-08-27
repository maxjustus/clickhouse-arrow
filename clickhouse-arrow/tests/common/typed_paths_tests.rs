use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::native::block_info::BlockInfo;
use clickhouse_arrow::native::types::Type;
use clickhouse_arrow::native::values::Value;

/// Generate test block with typed paths
pub fn generate_json_typed_paths_test_block() -> Block {
    let rows = vec![
        // Row with all paths
        Value::String(
            br#"{"id": 1, "name": "Alice", "password": "secret123", "score": 95.5, "active": true}"#
                .to_vec(),
        ),
        // Row with some paths missing  
        Value::String(
            br#"{"id": 2, "name": "Bob", "secret_key": "xyz", "score": 87.3}"#
                .to_vec(),
        ),
        // Row with different types for same paths
        Value::String(
            br#"{"id": 3, "name": "Charlie", "password": "hidden", "score": null, "tags": ["a", "b"]}"#
                .to_vec(),
        ),
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("json_col".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            skip_paths:        vec![
                "password".to_string(),
                "secret.*".to_string(), // Regex pattern
            ],
        })],
        column_data:  rows,
    }
}

/// Generate test block with skip paths
pub fn generate_json_skip_paths_test_block() -> Block {
    let rows = vec![
        Value::String(
            br#"{"public": "visible", "private": "hidden", "secret_key": "xyz", "data": {"nested": "value"}}"#
                .to_vec(),
        ),
        Value::String(
            br#"{"public": "data2", "private_info": "sensitive", "api_key": "abc123"}"#
                .to_vec(),
        ),
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("json_col".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_paths:        vec![
                "private.*".to_string(),
                "secret.*".to_string(),
                ".*_key".to_string(),
            ],
        })],
        column_data:  rows,
    }
}

/// Generate combined semi-evil test with typed paths, skip paths, and heterogeneous arrays
pub fn generate_json_semi_evil_test_block() -> Block {
    let rows = vec![
        // Complex nested with typed, skip, and heterogeneous arrays
        Value::String(
            br#"{"id": 42, "name": "Test", "password": "skip_me", "data": [1, "mixed", null, [true, 3.14]], "meta": {"active": true}}"#
                .to_vec(),
        ),
        // Different structure but same typed paths
        Value::String(
            br#"{"id": 99, "name": "Another", "secret_token": "hidden", "array": [[1,2], ["a","b"]], "other": 123}"#
                .to_vec(),
        ),
        // Typed path with wrong type (should handle gracefully)
        Value::String(
            br#"{"id": "not_a_number", "name": 456, "public": "visible", "nested": {"deep": {"value": [1, "two", 3]}}}"#
                .to_vec(),
        ),
    ];

    Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("json_col".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
                ("meta.active".to_string(), Box::new(Type::UInt8)), // Nested typed path
            ],
            skip_paths:        vec![
                "password".to_string(),
                "secret.*".to_string(),
                ".*token".to_string(),
            ],
        })],
        column_data:  rows,
    }
}
