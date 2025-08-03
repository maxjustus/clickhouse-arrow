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
async fn test_json_typed_nullability_defaults_e2e() {
    use crate::tests::run_test_with_cleanup;

    let result = run_test_with_cleanup(
        "test_json_typed_nullability_defaults_e2e",
        |container: Arc<ClickHouseContainer>| async move {
            // Create client
            let client = ClientBuilder::default()
                .with_endpoint(container.get_native_url())
                .with_username("clickhouse")
                .with_password("clickhouse")
                .build::<NativeFormat>()
                .await
                .unwrap();

            // Enable JSON/Object and allow LC without inner Nullable for typed JSON paths
            client.execute("SET allow_experimental_object_type = 1", None).await.unwrap();
            client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await.unwrap();

            // Prepare table
            client.execute("DROP TABLE IF EXISTS e2e_json_defaults", None).await.unwrap();
            client
                .execute(
                    r#"
        CREATE TABLE e2e_json_defaults (
            data JSON(
                id UInt32,
                status LowCardinality(String),
                value Variant(String, UInt64)
            )
        ) ENGINE = MergeTree() ORDER BY tuple()
        "#,
                    None,
                )
                .await
                .unwrap();

            // Build rows: some missing typed paths
            let rows = vec![
                Value::String(br#"{"name":"Alice"}"#.to_vec()),
                Value::String(br#"{"name":"Bob","id":5,"status":"ok"}"#.to_vec()),
                Value::String(br#"{"name":"Carol"}"#.to_vec()),
                Value::String(br#"{"name":"Dan","value":7}"#.to_vec()),
            ];

            let type_ = Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths:       vec![
                    ("id".to_string(), Box::new(Type::UInt32)),
                    ("status".to_string(), Box::new(Type::LowCardinality(Box::new(Type::String)))),
                    (
                        "value".to_string(),
                        Box::new(Type::variant(vec![Type::String, Type::UInt64])),
                    ),
                ],
                skip_exact:        vec![],
                skip_regex:        vec![],
            };

            let block = Block {
                info:         BlockInfo::default(),
                rows:         rows.len() as u64,
                column_types: vec![("data".to_string(), type_)],
                column_data:  rows,
            };

            // Insert using native
            use futures_util::StreamExt;
            let mut stream =
                client.insert("INSERT INTO e2e_json_defaults VALUES", block, None).await.unwrap();
            while let Some(res) = stream.next().await {
                res.unwrap();
            }

            // Verify row count
            #[derive(clickhouse_arrow_derive::Row, Debug)]
            struct Cnt {
                count: u64,
            }
            let mut cnt_stream = client
                .query::<Cnt>("SELECT count() AS count FROM e2e_json_defaults", None)
                .await
                .unwrap();
            let mut total = 0u64;
            while let Some(r) = cnt_stream.next().await {
                total += r.unwrap().count;
            }
            assert!(total > 0, "Expected rows inserted into e2e_json_defaults");

            // Select JSON as string for stable decoding
            #[derive(clickhouse_arrow_derive::Row, Debug)]
            struct RowOut {
                data: String,
            }
            let mut rows = client
                .query::<RowOut>("SELECT toJSONString(data) AS data FROM e2e_json_defaults", None)
                .await
                .unwrap();

            let mut seen: Vec<serde_json::Value> = Vec::new();
            while let Some(r) = rows.next().await {
                let r = r.unwrap();
                let v: serde_json::Value = serde_json::from_str(&r.data).unwrap();
                seen.push(v);
            }

            let get = |obj: &serde_json::Value, k: &str| {
                obj.get(k).cloned().unwrap_or(serde_json::Value::Null)
            };

            // 1) Non-nullable UInt32 defaults to 0 when missing
            let defaults_id = seen.iter().filter(|o| get(o, "id") == 0).count();
            assert!(defaults_id >= 2, "Expected at least two rows with id=0: {seen:?}");

            // 2) LowCardinality(String) defaults to empty string when missing
            let defaults_status = seen.iter().filter(|o| get(o, "status") == "").count();
            assert!(defaults_status >= 2, "Expected at least two rows with status=\"\" : {seen:?}");

            // 3) Variant missing should be JSON null; present should be number (or a stringified
            //    number)
            assert!(seen.iter().any(|o| get(o, "value") == serde_json::Value::Null));
            assert!(
                seen.iter().any(|o| {
                    let v = get(o, "value");
                    match v {
                        serde_json::Value::Number(n) => n.as_u64() == Some(7),
                        serde_json::Value::String(ref s) => s == "7",
                        _ => false,
                    }
                }),
                "Expected value=7 (number or string): Seen rows: {:?}",
                seen
            );

            // Cleanup
            client.execute("DROP TABLE e2e_json_defaults", None).await.unwrap();
        },
        None,
        None,
    )
    .await;

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
