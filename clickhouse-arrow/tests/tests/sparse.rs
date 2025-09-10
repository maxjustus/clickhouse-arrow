use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::InsertOptions;
use futures_util::StreamExt;
use clickhouse_arrow::test_utils::ClickHouseContainer;

// Verify we can read sparse/custom Float32 columns correctly end-to-end.
pub async fn test_sparse_float32_e2e(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    client.execute("DROP TABLE IF EXISTS e2e_sparse_f32", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_sparse_f32 (x Float32) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    // Insert 10 rows; only 2 are non-default to trigger sparse on server
    let mut op = client
        .insert_into("e2e_sparse_f32", InsertOptions::default())
        .await
        .expect("insert_into");
    let rows = vec![
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 3000.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 30000.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 0.0}),
        serde_json::json!({"x": 0.0}),
    ];
    let _ = op.write_rows(rows).await.expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        x: f32,
    }
    let mut rs = client
        .query::<RowOut>("SELECT x FROM e2e_sparse_f32 ORDER BY tuple() LIMIT 10", None)
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rs.next().await { got.push(r.expect("row")); }
    assert_eq!(got.len(), 10);
    let vals: Vec<f32> = got.into_iter().map(|r| r.x).collect();
    assert_eq!(vals, vec![0.0, 3000.0, 0.0, 0.0, 0.0, 30000.0, 0.0, 0.0, 0.0, 0.0]);

    client.execute("DROP TABLE e2e_sparse_f32", None).await.expect("drop");
}
