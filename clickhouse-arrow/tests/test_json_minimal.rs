#![allow(unused_extern_crates)]
pub mod common;
pub mod tests;

use std::sync::Arc;

use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::native::block_info::BlockInfo;
use clickhouse_arrow::native::types::Type;
use clickhouse_arrow::native::values::Value;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::*;

#[tokio::test]
async fn test_json_minimal() {
    use crate::tests::run_test_with_cleanup;

    let result = run_test_with_cleanup(
        "test_json_minimal",
        |container: Arc<ClickHouseContainer>| async move {
            // Create client
            let client = ClientBuilder::default()
                .with_endpoint(container.get_native_url())
                .with_username("clickhouse")
                .with_password("clickhouse")
                .build::<NativeFormat>()
                .await
                .unwrap();

            // Create simple table with JSON column
            client.execute("DROP TABLE IF EXISTS test_json_min", None).await.unwrap();
            client
                .execute(
                    "CREATE TABLE test_json_min (data JSON) ENGINE = MergeTree() ORDER BY tuple()",
                    None,
                )
                .await
                .unwrap();

            // Try to insert using native protocol with our JSON serialization
            // Provide valid JSON bytes (raw string, no escaping inside)
            let json_values = vec![Value::String(br#"{"id": 1, "name": "test"}"#.to_vec())];

            let block = Block {
                info:         BlockInfo::default(),
                rows:         1,
                column_types: vec![("data".to_string(), Type::JSON {
                    max_dynamic_paths: None,
                    max_dynamic_types: None,
                    typed_paths:       vec![],
                    skip_exact:        vec![],
                    skip_regex:        vec![],
                })],
                column_data:  json_values,
            };

            use futures_util::StreamExt;
            let mut stream =
                client.insert("INSERT INTO test_json_min VALUES", block, None).await.unwrap();
            while let Some(result) = stream.next().await {
                result.unwrap();
            }

            // Clean up
            client.execute("DROP TABLE test_json_min", None).await.unwrap();
        },
        None,
        None,
    )
    .await;

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
