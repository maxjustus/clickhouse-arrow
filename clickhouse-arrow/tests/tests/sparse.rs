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

// Verify nested Tuple sparse kinds are handled end-to-end.
pub async fn test_sparse_tuple_nested_e2e(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    client.execute("DROP TABLE IF EXISTS e2e_sparse_tuple_nested", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_sparse_tuple_nested (t Tuple(UInt64, Tuple(UUID, UInt64))) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    // Insert 10 rows with mostly default values to encourage SPARSE for leaves
    // Using explicit SQL VALUES for clarity
    let insert_sql = r#"
        INSERT INTO e2e_sparse_tuple_nested VALUES
          ((0,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((1,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((0,  ('11111111-2222-3333-4444-555555555555', 0))),
          ((0,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((0,  ('00000000-0000-0000-0000-000000000000', 999))),
          ((2,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((0,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((0,  ('00000000-0000-0000-0000-000000000000', 0))),
          ((0,  ('aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee', 0))),
          ((0,  ('00000000-0000-0000-0000-000000000000', 0)))
    "#;
    client.execute(insert_sql, None).await.expect("insert");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        o: u64,
        u: uuid::Uuid,
        i: u64,
    }
    let mut rs = client
        .query::<RowOut>(
            "SELECT t.1 AS o, t.2.1 AS u, t.2.2 AS i FROM e2e_sparse_tuple_nested LIMIT 10",
            None,
        )
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rs.next().await { got.push(r.expect("row")); }
    assert_eq!(got.len(), 10);

    // Validate a few key rows
    assert_eq!(got[0].o, 0);
    assert_eq!(got[1].o, 1);
    assert_eq!(got[2].u.to_string(), "11111111-2222-3333-4444-555555555555");
    assert_eq!(got[4].i, 999);
    assert_eq!(got[5].o, 2);
    assert_eq!(got[8].u.to_string(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

    client
        .execute("DROP TABLE e2e_sparse_tuple_nested", None)
        .await
        .expect("drop");
}
