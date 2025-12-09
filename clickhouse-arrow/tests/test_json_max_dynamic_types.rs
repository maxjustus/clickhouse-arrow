#![allow(unused_extern_crates)]
#![cfg(feature = "test-utils")]
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
async fn test_json_max_dynamic_types_server_selection() {
    use crate::tests::run_test_with_cleanup;

    let result = run_test_with_cleanup(
        "test_json_max_dynamic_types_server_selection",
        |container: Arc<ClickHouseContainer>| async move {
            let client = ClientBuilder::default()
                .with_endpoint(container.get_native_url())
                .with_username("clickhouse")
                .with_password("clickhouse")
                .build::<NativeFormat>()
                .await
                .unwrap();

            client.execute("DROP TABLE IF EXISTS e2e_json_max_dyn_types", None).await.unwrap();
            client
                .execute(
                    r#"
        CREATE TABLE e2e_json_max_dyn_types (
            data JSON(max_dynamic_types=2)
        ) ENGINE = MergeTree() ORDER BY tuple()
        "#,
                    None,
                )
                .await
                .unwrap();

            let rows = vec![
                Value::String(br#"{"value": "str", "other": 1}"#.to_vec()),
                Value::String(br#"{"value": 42}"#.to_vec()),
                Value::String(br#"{"value": 3.14159}"#.to_vec()),
                Value::String(br#"{"value": [1, 2, 3]}"#.to_vec()),
            ];

            let type_ = Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: Some(2),
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            };

            let block = Block {
                info: BlockInfo::default(),
                rows: rows.len() as u64,
                column_types: vec![("data".to_string(), type_)],
                column_data: rows.clone(),
                ..Default::default()
            };

            use futures_util::StreamExt;
            let mut stream = client
                .insert("INSERT INTO e2e_json_max_dyn_types VALUES", block, None)
                .await
                .unwrap();
            while let Some(r) = stream.next().await {
                r.unwrap();
            }

            #[derive(clickhouse_arrow_derive::Row, Debug)]
            struct Cnt {
                count: u64,
            }
            let mut cnt_stream = client
                .query::<Cnt>("SELECT count() AS count FROM e2e_json_max_dyn_types", None)
                .await
                .unwrap();
            let mut total = 0u64;
            while let Some(r) = cnt_stream.next().await {
                total += r.unwrap().count;
            }
            assert_eq!(total, 4, "expected 4 rows inserted");

            #[derive(clickhouse_arrow_derive::Row, Debug)]
            struct RowOut {
                data: String,
            }
            let mut rows_out = client
                .query::<RowOut>(
                    "SELECT toJSONString(data) AS data FROM e2e_json_max_dyn_types ORDER BY data",
                    None,
                )
                .await
                .unwrap();
            let mut seen = Vec::new();
            while let Some(r) = rows_out.next().await {
                seen.push(r.unwrap().data);
            }
            assert_eq!(seen.len(), 4);

            client.execute("DROP TABLE e2e_json_max_dyn_types", None).await.unwrap();
        },
        None,
        None,
    )
    .await;

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
