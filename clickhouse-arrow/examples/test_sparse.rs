use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::{ClickHouseContainer, get_shared_container};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Get test container
    let container: Arc<ClickHouseContainer> = get_shared_container().await;

    // Create client
    let client = ClientBuilder::default()
        .with_endpoint(container.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await?;

    println!("Testing sparse column deserialization...");

    // First create a table with sparse column likely scenario
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS test_sparse (id UInt64, sparse_text String) ENGINE = \
             MergeTree ORDER BY id",
            None,
        )
        .await?;

    // Insert data where sparse_text is mostly empty (will trigger sparse serialization)
    let mut insert_query = String::from("INSERT INTO test_sparse VALUES ");
    for i in 0..10000 {
        if i > 0 {
            insert_query.push_str(", ");
        }
        if i % 100 == 0 {
            // Only 1% of rows have non-empty string
            insert_query.push_str(&format!("({}, 'value_{}')", i, i));
        } else {
            // 99% have empty string (sparse)
            insert_query.push_str(&format!("({}, '')", i));
        }
    }

    client.execute(&insert_query, None).await?;

    // Now query it back - this should trigger sparse deserialization
    let query = "SELECT * FROM test_sparse";

    match client.query_raw(query.to_string(), None::<Vec<(String, String)>>, Qid::new()).await {
        Ok(mut stream) => {
            println!("Query succeeded!");
            let mut count = 0;
            while let Some(block_res) = stream.next().await {
                let block = block_res?;
                count += block.rows;
            }
            println!("Read {} rows successfully", count);
        }
        Err(e) => {
            println!("Query failed with error: {:?}", e);
        }
    }

    Ok(())
}
