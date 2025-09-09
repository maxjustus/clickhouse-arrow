#![allow(unused_extern_crates)]
use std::sync::Arc;

use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::native::block_info::BlockInfo;
use clickhouse_arrow::native::types::Type;
use clickhouse_arrow::native::values::Value;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::{ClickHouseContainer, get_shared_container};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get test container
    let container: Arc<ClickHouseContainer> = get_shared_container().await;

    // Create client
    let client = ClientBuilder::default()
        .with_endpoint(container.get_native_url())
        .build::<NativeFormat>()
        .await?;

    // Drop and create test table with JSON column and typed paths
    client.execute("DROP TABLE IF EXISTS test_json_typed_debug", None).await?;

    // Test with both typed and dynamic paths
    client
        .execute(
            "CREATE TABLE test_json_typed_debug (
            data JSON(
                id UInt32,
                name String
            )
        ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await?;

    println!("Table created successfully");

    // Create test block with mixed typed and dynamic fields
    let rows = vec![
        Value::String(br#"{"id": 1, "name": "Alice", "age": 30}"#.to_vec()),
        Value::String(br#"{"id": 2, "name": "Bob", "score": 95.5}"#.to_vec()),
    ];

    let block = Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("data".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            skip_exact:        vec![],
            skip_regex:        vec![],
        })],
        column_data:  rows,
    };

    println!("Inserting block with {} rows", block.rows);

    // Try to insert
    use futures_util::StreamExt;
    let mut stream = client.insert("INSERT INTO test_json_typed_debug VALUES", block, None).await?;

    println!("Waiting for insert stream...");
    while let Some(result) = stream.next().await {
        println!("Got stream result");
        result?;
    }

    println!("Insert successful!");

    // Query count to verify insertion worked
    println!("Checking if data was inserted...");
    client.execute("SELECT count() FROM test_json_typed_debug", None).await?;
    println!("Query successful!");

    // Clean up
    client.execute("DROP TABLE test_json_typed_debug", None).await?;
    println!("Table dropped");

    Ok(())
}
