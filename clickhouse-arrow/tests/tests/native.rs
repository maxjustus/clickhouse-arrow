use std::sync::Arc;

use clickhouse_arrow::CompressionMethod;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use futures_util::StreamExt;
use clickhouse_arrow::InsertOptions;

use crate::common::native_helpers::*;

// Helper struct for type check queries
#[derive(Debug, Clone, Row)]
#[allow(dead_code)]
struct TypeCheckRow {
    dtype: String,
}

// Helper functions to reduce repetitive patterns

/// # Panics
pub async fn test_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_compression(CompressionMethod::LZ4);
    let block = generate_test_block();

    harness
        .run_native_roundtrip_test("test_round_trip", &block)
        .await
        .expect("Round trip failed");
}

/// # Panics
pub async fn test_variant_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_compression(CompressionMethod::LZ4);
    let block = generate_variant_test_block();

    harness
        .run_native_roundtrip_test("test_variant_round_trip", &block)
        .await
        .expect("Variant round trip failed");
}

/// Tests dynamic type round-trip serialization.
///
/// # Panics
/// Panics if the dynamic round trip test fails.
pub async fn test_dynamic_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_dynamic_test_block();

    harness
        .run_native_roundtrip_test("test_dynamic_round_trip", &block)
        .await
        .expect("Dynamic round trip failed");
}

/// Tests JSON type round-trip serialization.
///
/// # Panics
/// Panics if the JSON round trip test fails.
pub async fn test_json_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_json_test_block();

    harness
        .run_native_roundtrip_test("test_json_round_trip", &block)
        .await
        .expect("JSON round trip failed");
}

/// Tests JSON with arrays round-trip serialization.
///
/// # Panics
/// Panics if the JSON array round trip test fails.
pub async fn test_json_arrays(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_json_array_test_block();

    harness
        .run_native_roundtrip_test("test_json_arrays", &block)
        .await
        .expect("JSON array round trip failed");
}

/// Tests mixed dynamic and JSON type round-trip serialization.
///
/// # Panics
/// Panics if the mixed dynamic JSON round trip test fails.
pub async fn test_mixed_dynamic_json(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_mixed_dynamic_json_test_block();

    harness
        .run_native_roundtrip_test("test_mixed_dynamic_json", &block)
        .await
        .expect("Mixed dynamic JSON round trip failed");
}

/// Tests evil heterogeneous arrays in dynamic type round-trip serialization.
///
/// # Panics
/// Panics if the evil heterogeneous dynamic round trip test fails.
pub async fn test_evil_heterogeneous_dynamic(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_evil_heterogeneous_dynamic_test_block();

    harness
        .run_native_roundtrip_test("test_evil_heterogeneous_dynamic", &block)
        .await
        .expect("Evil heterogeneous dynamic round trip failed");
}

/// Tests evil heterogeneous arrays in JSON type round-trip serialization.
///
/// # Panics
/// Panics if the evil heterogeneous JSON round trip test fails.
pub async fn test_evil_heterogeneous_json(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_evil_heterogeneous_json_test_block();

    harness
        .run_native_roundtrip_test("test_evil_heterogeneous_json", &block)
        .await
        .expect("Evil heterogeneous JSON round trip failed");
}

/// Tests JSON with Variant typed paths round-trip serialization.
///
/// # Panics
/// Panics if the JSON Variant typed paths round trip test fails.
pub async fn test_json_typed_paths_variant(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_json_typed_paths_variant_test_block();

    harness
        .run_native_roundtrip_test("test_json_typed_paths_variant", &block)
        .await
        .expect("JSON Variant typed paths round trip failed");
}

/// Tests insert_into with a simple non-JSON multi-column table
///
/// # Panics
pub async fn test_insert_into_nonjson_multi(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    client.execute("DROP TABLE IF EXISTS e2e_nonjson_multi", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_nonjson_multi (id UInt32, name String) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client.insert_into("e2e_nonjson_multi", InsertOptions::default()).await.expect("insert_into");
    op.write_rows(vec![
        serde_json::json!({"id": 1, "name": "Alice"}),
        serde_json::json!({"name": "Bob"}), // id missing -> default 0
    ])
    .await
    .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut { id: u32, name: String }
    let mut rows = client
        .query::<RowOut>("SELECT id, name FROM e2e_nonjson_multi ORDER BY name", None)
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rows.next().await { got.push(r.expect("row")); }
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "Alice");
    assert_eq!(got[0].id, 1);
    assert_eq!(got[1].name, "Bob");
    assert_eq!(got[1].id, 0); // defaulted

    client.execute("DROP TABLE e2e_nonjson_multi", None).await.expect("drop");
}

/// Tests insert_into with mixed non-JSON + JSON typed paths
///
/// # Panics
pub async fn test_insert_into_mixed_json(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await.ok();

    client.execute("DROP TABLE IF EXISTS e2e_mixed_json", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_mixed_json (
                ts UInt32,
                data JSON(
                    id UInt32,
                    status LowCardinality(String)
                )
            ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client.insert_into("e2e_mixed_json", InsertOptions::default()).await.expect("insert_into");
    op.write_rows(vec![
        serde_json::json!({"ts": 1, "data": {"id": 5, "status": "ok"}}),
        serde_json::json!({"ts": 2, "data": {"name": "missing_typed"}}),
        serde_json::json!({"ts": 3}), // missing data -> Null
    ])
    .await
    .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut { ts: u64, data: String }
    let mut rows = client
        .query::<RowOut>("SELECT ts, toJSONString(data) as data FROM e2e_mixed_json ORDER BY ts", None)
        .await
        .expect("select");
    let mut seen = Vec::new();
    while let Some(r) = rows.next().await { seen.push(r.expect("row")); }
    assert_eq!(seen.len(), 3);
    // Parse JSON and assert id/status defaults
    let parse = |s: &str| -> serde_json::Value { serde_json::from_str(s).unwrap() };
    let v0 = parse(&seen[0].data);
    assert_eq!(seen[0].ts, 1);
    assert_eq!(v0.get("id").cloned().unwrap_or(serde_json::Value::Null), serde_json::Value::from(5u64));
    assert_eq!(v0.get("status").cloned().unwrap_or(serde_json::Value::Null), serde_json::Value::from("ok"));

    let v1 = parse(&seen[1].data);
    assert_eq!(seen[1].ts, 2);
    // Missing typed id/status should default
    assert_eq!(v1.get("id").cloned().unwrap_or(serde_json::Value::Null), serde_json::Value::from(0u64));
    assert_eq!(v1.get("status").cloned().unwrap_or(serde_json::Value::Null), serde_json::Value::from(""));

    let v2 = parse(&seen[2].data);
    assert_eq!(seen[2].ts, 3);
    // data was Null; toJSONString renders it as {}
    assert!(v2.is_object());

    client.execute("DROP TABLE e2e_mixed_json", None).await.expect("drop");
}

/// Moves standalone JSON minimal test into the common e2e harness using insert_into
///
/// # Panics
pub async fn test_json_minimal_insert_into(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    client.execute("DROP TABLE IF EXISTS test_json_min", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE test_json_min (data JSON) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client.insert_into("test_json_min", InsertOptions::default()).await.expect("insert_into");
    op.write_rows(vec![serde_json::json!({"data": {"id": 1, "name": "test"}})])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    // Simple readback
    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut { data: String }
    let mut rows = client
        .query::<RowOut>("SELECT toJSONString(data) as data FROM test_json_min", None)
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rows.next().await { got.push(r.expect("row")); }
    assert_eq!(got.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&got[0].data).unwrap();
    // Accept numeric or stringified numeric for id (server JSON rendering may vary)
    let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
    match id {
        serde_json::Value::Number(n) => assert_eq!(n.as_u64(), Some(1)),
        serde_json::Value::String(s) => assert_eq!(s, "1"),
        other => panic!("Unexpected id value: {other:?}"),
    }
    assert_eq!(v.get("name").unwrap(), &serde_json::Value::from("test"));

    client.execute("DROP TABLE test_json_min", None).await.expect("drop");
}
