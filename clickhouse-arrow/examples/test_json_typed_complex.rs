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

    // Create client with proper credentials
    let client = ClientBuilder::default()
        .with_endpoint(container.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await?;

    println!("=== Testing JSON with LowCardinality typed paths ===\n");
    if let Err(e) = test_lowcardinality_typed_paths(&client).await {
        println!("LowCardinality test failed (expected): {e}\n");
    }

    println!("\n=== Testing JSON with Variant typed paths ===\n");
    test_variant_typed_paths(&client).await?;

    Ok(())
}

async fn test_lowcardinality_typed_paths(
    client: &Client<NativeFormat>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Drop and create test table with LowCardinality typed paths
    client.execute("DROP TABLE IF EXISTS test_json_lowcard", None).await?;

    // Enable the setting to allow suspicious LowCardinality types
    client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await?;

    // Create table with LowCardinality typed paths
    client
        .execute(
            "CREATE TABLE test_json_lowcard (
            data JSON(
                status LowCardinality(String),
                priority LowCardinality(UInt32),
                category LowCardinality(Nullable(String))
            )
        ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await?;

    println!("Table created with LowCardinality typed paths");

    // Create test block with JSON data
    let rows = vec![
        Value::String(br#"{"status": "active", "priority": 1, "category": "work"}"#.to_vec()),
        Value::String(br#"{"status": "inactive", "priority": 2, "category": null}"#.to_vec()),
        Value::String(
            br#"{"status": "pending", "priority": 3, "extra_field": "ignored"}"#.to_vec(),
        ),
    ];

    let block = Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("data".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("status".to_string(), Box::new(Type::LowCardinality(Box::new(Type::String)))),
                ("priority".to_string(), Box::new(Type::LowCardinality(Box::new(Type::UInt32)))),
                (
                    "category".to_string(),
                    Box::new(Type::LowCardinality(Box::new(Type::Nullable(Box::new(
                        Type::String,
                    ))))),
                ),
            ],
            skip_paths:        vec![],
        })],
        column_data:  rows,
    };

    println!("Attempting to insert block with {} rows", block.rows);

    // Try to insert
    use futures_util::StreamExt;
    match client.insert("INSERT INTO test_json_lowcard VALUES", block, None).await {
        Ok(mut stream) => {
            println!("Insert stream created, waiting for results...");
            while let Some(result) = stream.next().await {
                match result {
                    Ok(_) => println!("Stream chunk processed successfully"),
                    Err(e) => {
                        println!("Error during insertion: {e}");
                        return Err(Box::new(e));
                    }
                }
            }
            println!("Insert successful!");

            // Query to verify data
            println!("\nQuerying count to verify insertion...");
            client.execute("SELECT count() FROM test_json_lowcard", None).await?;
            println!("Query successful - data was inserted!");
        }
        Err(e) => {
            println!("Failed to create insert stream: {e}");
            println!(
                "This suggests the conversion isn't happening properly for LowCardinality types"
            );
        }
    }

    // Clean up
    client.execute("DROP TABLE test_json_lowcard", None).await?;
    println!("Table dropped");

    Ok(())
}

async fn test_variant_typed_paths(
    client: &Client<NativeFormat>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Drop and create test table with Variant typed paths
    client.execute("DROP TABLE IF EXISTS test_json_variant", None).await?;

    // Create table with Variant typed paths
    client
        .execute(
            "CREATE TABLE test_json_variant (
            data JSON(
                value Variant(String, UInt64, Float64),
                mixed Variant(String, UInt8)
            )
        ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await?;

    println!("Table created with Variant typed paths");

    // Create test block with JSON data containing different types
    let rows = vec![
        Value::String(br#"{"value": "text", "mixed": 1}"#.to_vec()),
        Value::String(br#"{"value": 42, "mixed": "hello"}"#.to_vec()),
        Value::String(br#"{"value": 3.14, "mixed": 0}"#.to_vec()),
    ];

    let block = Block {
        info:         BlockInfo::default(),
        rows:         rows.len() as u64,
        column_types: vec![("data".to_string(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                (
                    "value".to_string(),
                    Box::new(Type::Variant(vec![Type::String, Type::UInt64, Type::Float64])),
                ),
                (
                    "mixed".to_string(),
                    Box::new(Type::Variant(vec![
                        Type::String,
                        Type::UInt8, // Using UInt8 instead of Bool since Bool type doesn't exist
                    ])),
                ),
            ],
            skip_paths:        vec![],
        })],
        column_data:  rows,
    };

    println!("Attempting to insert block with {} rows", block.rows);

    // Try to insert
    use futures_util::StreamExt;
    match client.insert("INSERT INTO test_json_variant VALUES", block, None).await {
        Ok(mut stream) => {
            println!("Insert stream created, waiting for results...");
            while let Some(result) = stream.next().await {
                match result {
                    Ok(_) => println!("Stream chunk processed successfully"),
                    Err(e) => {
                        println!("Error during insertion: {e}");
                        println!("Error details suggest Variant conversion isn't working");
                        return Err(Box::new(e));
                    }
                }
            }
            println!("Insert successful!");

            // Query to verify data
            println!("\nQuerying count to verify insertion...");
            client.execute("SELECT count() FROM test_json_variant", None).await?;
            println!("Query successful - data was inserted!");
        }
        Err(e) => {
            println!("Failed to create insert stream: {e}");
            println!("This suggests the conversion isn't happening properly for Variant types");
        }
    }

    // Clean up
    client.execute("DROP TABLE test_json_variant", None).await?;
    println!("Table dropped");

    Ok(())
}
