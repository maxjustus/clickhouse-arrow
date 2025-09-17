use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod, InsertOptions};
use futures_util::StreamExt;

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
            "CREATE TABLE e2e_nonjson_multi (id UInt32, name String) ENGINE = MergeTree() ORDER \
             BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client
        .insert_into("e2e_nonjson_multi", InsertOptions::default())
        .await
        .expect("insert_into");
    let _ = op
        .write_rows(vec![
            serde_json::json!({"id": 1, "name": "Alice"}),
            serde_json::json!({"name": "Bob"}), // id missing -> default 0
        ])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        id:   u32,
        name: String,
    }
    let mut rows = client
        .query::<RowOut>("SELECT id, name FROM e2e_nonjson_multi ORDER BY name", None)
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rows.next().await {
        got.push(r.expect("row"));
    }
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
    // TODO: put this code block in something reusable. It feels weird we have the roundtrip
    // harness helper but then use this in places.
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    // TODO: I would love if this setting could be auto-applied if ch version >= 25.6
    let _ = client
        .execute("SET output_format_native_use_flattened_dynamic_and_json_serialization = 1", None)
        .await
        .ok();
    let _ = client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await.ok();

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

    let mut op =
        client.insert_into("e2e_mixed_json", InsertOptions::default()).await.expect("insert_into");
    let _ = op
        .write_rows(vec![
            serde_json::json!({"ts": 1, "data": {"id": 5, "status": "ok"}}),
            serde_json::json!({"ts": 2, "data": {"name": "missing_typed"}}),
            serde_json::json!({"ts": 3}), // missing data -> Null
        ])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        ts:   u32,
        data: serde_json::Value,
    }
    let mut rows = client
        .query::<RowOut>("SELECT ts, data as data FROM e2e_mixed_json ORDER BY ts", None)
        .await
        .expect("select");
    let mut seen = Vec::new();
    while let Some(r) = rows.next().await {
        seen.push(r.expect("row"));
    }
    assert_eq!(seen.len(), 3);
    let v0 = &seen[0].data;
    assert_eq!(seen[0].ts, 1);
    assert_eq!(
        v0.get("id").cloned().unwrap_or(serde_json::Value::Null),
        serde_json::Value::from(5u64)
    );
    assert_eq!(
        v0.get("status").cloned().unwrap_or(serde_json::Value::Null),
        serde_json::Value::from("ok")
    );

    let v1 = &seen[1].data;
    assert_eq!(seen[1].ts, 2);
    // Missing typed id/status should default
    assert_eq!(
        v1.get("id").cloned().unwrap_or(serde_json::Value::Null),
        serde_json::Value::from(0u64)
    );
    assert_eq!(
        v1.get("status").cloned().unwrap_or(serde_json::Value::Null),
        serde_json::Value::from("")
    );

    let v2 = &seen[2].data;
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

    let _ = client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    client.execute("DROP TABLE IF EXISTS test_json_min", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE test_json_min (data JSON) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op =
        client.insert_into("test_json_min", InsertOptions::default()).await.expect("insert_into");
    let _ = op
        .write_rows(vec![serde_json::json!({"data": {"id": 1, "name": "test"}})])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    // Simple readback
    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        data: String,
    }
    let mut rows = client
        .query::<RowOut>("SELECT toJSONString(data) as data FROM test_json_min", None)
        .await
        .expect("select");
    let mut got = Vec::new();
    while let Some(r) = rows.next().await {
        got.push(r.expect("row"));
    }
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

/// Tests direct serde deserialization for JSON columns using Json<T>
///
/// # Panics
pub async fn test_json_direct_deserialize(ch: Arc<ClickHouseContainer>) {
    use clickhouse_arrow::native::values::json::Json;
    use serde::Deserialize;

    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    // Ensure JSON/Object allowed
    let _ = client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    let _ = client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await.ok();

    client.execute("DROP TABLE IF EXISTS e2e_json_direct", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_json_direct (
                data JSON(id UInt32, name String)
            ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client
        .insert_into("e2e_json_direct", InsertOptions::default())
        .await
        .expect("insert_into");
    let _ = op
        .write_rows(vec![
            serde_json::json!({"data": {"id": 1, "name": "Alice"}}),
            serde_json::json!({"data": {"name": "Bob"}}),
        ])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(Deserialize, serde::Serialize, Debug, PartialEq, Eq)]
    struct Data {
        id:   u32,
        name: String,
    }
    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        data: Json<Data>,
    }

    let mut rows = client
        .query::<RowOut>(
            "SELECT toJSONString(data) as data FROM e2e_json_direct ORDER BY toJSONString(data)",
            None,
        )
        .await
        .expect("select");
    let mut seen = Vec::new();
    while let Some(r) = rows.next().await {
        seen.push(r.expect("row"));
    }
    // Some ClickHouse versions may return non-deterministic ordering for JSON projections;
    // enforce deterministic order in test to validate values regardless of server specifics.
    seen.sort_by(|a, b| a.data.0.name.cmp(&b.data.0.name));
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].data.0, Data { id: 1, name: "Alice".to_string() });
    // Missing typed id defaults; present fields are preserved
    assert_eq!(seen[1].data.0, Data { id: 0, name: "Bob".to_string() });

    client.execute("DROP TABLE e2e_json_direct", None).await.expect("drop");
}

/// Tests direct serde_json::Value deserialization for JSON columns
///
/// # Panics
pub async fn test_json_direct_dynamic(ch: Arc<ClickHouseContainer>) {
    use crate::common::version_compat::VersionChecker;

    // Check if ClickHouse supports JSON v3 format (requires direct JSON column reading)
    let version_checker = VersionChecker::new(std::env::var("CLICKHOUSE_VERSION").ok().as_deref());
    if !version_checker.require_json_v3_support("test_json_direct_dynamic") {
        return; // Skip test on unsupported versions
    }

    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    let _ = client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    let _ = client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await.ok();

    client.execute("DROP TABLE IF EXISTS e2e_json_direct_dyn", None).await.expect("drop");
    client
        .execute(
            "CREATE TABLE e2e_json_direct_dyn (
                data JSON(
                    a UInt32
                )
            ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    let mut op = client
        .insert_into("e2e_json_direct_dyn", InsertOptions::default())
        .await
        .expect("insert_into");
    let _ = op
        .write_rows(vec![serde_json::json!({"data": {"a": 42}}), serde_json::json!({"data": {}})])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    #[derive(clickhouse_arrow_derive::Row, Debug)]
    struct RowOut {
        data: serde_json::Value,
    }
    let mut rows = client
        .query::<RowOut>("SELECT data FROM e2e_json_direct_dyn ORDER BY toString(data)", None)
        .await
        .expect("select");
    let mut seen = Vec::new();
    while let Some(r) = rows.next().await {
        seen.push(r.expect("row"));
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].data["a"].as_u64(), Some(42));
    // Missing typed path defaults to 0
    assert_eq!(seen[1].data["a"].as_u64(), Some(0));

    client.execute("DROP TABLE e2e_json_direct_dyn", None).await.expect("drop");
}

/// Tests query_json method for dynamic row reading
///
/// # Panics
pub async fn test_query_json_basic(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    // Test with a simple query - SELECT values with different types
    let rows = client
        .query_json("SELECT 42 as id, 'Alice' as name, true as active, 3.14 as score", None)
        .await
        .expect("query_json");

    assert_eq!(rows.len(), 1);

    // Check the row values and types
    assert_eq!(rows[0]["id"].as_u64(), Some(42));
    assert_eq!(rows[0]["name"].as_str(), Some("Alice"));

    // ClickHouse stores Bool as UInt8, so check for number instead of bool
    assert_eq!(rows[0]["active"].as_u64(), Some(1));

    // Float comparison with tolerance
    let score = rows[0]["score"].as_f64().expect("score should be a number");
    assert!((score - 3.14).abs() < 0.001);

    // Test with multiple rows using UNION ALL
    let rows = client
        .query_json(
            "SELECT 1 as id, 'Alice' as name UNION ALL SELECT 2 as id, 'Bob' as name ORDER BY id",
            None,
        )
        .await
        .expect("query_json");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"].as_u64(), Some(1));
    assert_eq!(rows[0]["name"].as_str(), Some("Alice"));
    assert_eq!(rows[1]["id"].as_u64(), Some(2));
    assert_eq!(rows[1]["name"].as_str(), Some("Bob"));

    // Test with arrays and explicit nullable (cast NULL to a specific type)
    let rows = client
        .query_json(
            "SELECT [1, 2, 3] as numbers, CAST(NULL AS Nullable(String)) as empty_field",
            None,
        )
        .await
        .expect("query_json");

    assert_eq!(rows.len(), 1);
    assert!(rows[0]["numbers"].is_array());
    assert!(rows[0]["empty_field"].is_null());
}

/// Tests round-trip functionality: insert serde_json::Map into an Object('json') column and read
/// back with query_json
pub async fn test_legacy_object_json_map_roundtrip(ch: Arc<ClickHouseContainer>) {
    let client = ClientBuilder::default()
        .with_endpoint(ch.get_native_url())
        .with_username("clickhouse")
        .with_password("clickhouse")
        .build::<NativeFormat>()
        .await
        .expect("build client");

    let _ = client.execute("SET allow_experimental_object_type = 1", None).await.ok();
    client
        .execute(
            "CREATE OR REPLACE TABLE e2e_json_map_roundtrip (
                id UInt32,
                data Object('json')
            ) ENGINE = MergeTree() ORDER BY tuple()",
            None,
        )
        .await
        .expect("create");

    // Create serde_json::Map objects for insertion
    let mut row1_data = serde_json::Map::new();
    let _previous = row1_data.insert("name".to_string(), serde_json::json!("Alice"));
    let _previous = row1_data.insert("age".to_string(), serde_json::json!(30));
    let _previous = row1_data.insert("active".to_string(), serde_json::json!(true));

    let mut row2_data = serde_json::Map::new();
    let _previous = row2_data.insert("name".to_string(), serde_json::json!("Bob"));
    let _previous = row2_data.insert("age".to_string(), serde_json::json!(25));
    let _previous =
        row2_data.insert("hobbies".to_string(), serde_json::json!(["coding", "reading"]));

    // Insert using serde_json::Map
    let mut op = client
        .insert_into("e2e_json_map_roundtrip", InsertOptions::default())
        .await
        .expect("insert_into");
    let _ = op
        .write_rows(vec![
            serde_json::json!({"id": 1, "data": row1_data}),
            serde_json::json!({"id": 2, "data": row2_data}),
        ])
        .await
        .expect("write_rows");
    op.finish().await.expect("finish");

    // Query back using query_json
    let rows = client
        .query_json("SELECT id, data FROM e2e_json_map_roundtrip ORDER BY id", None)
        .await
        .expect("query_json");

    assert_eq!(rows.len(), 2);

    // Verify row 1
    assert_eq!(rows[0]["id"].as_u64(), Some(1));
    let data1 = &rows[0]["data"];
    // NOTE: ClickHouse Object('json') columns get converted to concrete Tuple types
    // based on the data structure, so they come back as arrays, not objects.
    // The tuple structure from the debug was: Tuple([UInt8, Int8, Array(String), String])
    // which corresponds to: [active, age, hobbies, name] in alphabetical order
    assert!(data1.is_array());
    let data1_array = data1.as_array().unwrap();
    assert_eq!(data1_array.len(), 4);
    // Fields are alphabetically ordered: active, age, hobbies, name
    assert_eq!(data1_array[0].as_u64(), Some(1)); // active: true -> 1
    assert_eq!(data1_array[1].as_i64(), Some(30)); // age: 30
    assert!(data1_array[2].is_array() && data1_array[2].as_array().unwrap().is_empty()); // hobbies: []
    assert_eq!(data1_array[3].as_str(), Some("Alice")); // name: "Alice"

    // Verify row 2
    assert_eq!(rows[1]["id"].as_u64(), Some(2));
    let data2 = &rows[1]["data"];
    assert!(data2.is_array());
    let data2_array = data2.as_array().unwrap();
    assert_eq!(data2_array.len(), 4);
    // Fields are alphabetically ordered: active, age, hobbies, name
    assert_eq!(data2_array[0].as_u64(), Some(0)); // active: undefined/null -> 0
    assert_eq!(data2_array[1].as_i64(), Some(25)); // age: 25
    let hobbies = &data2_array[2];
    assert!(hobbies.is_array());
    let hobbies_array = hobbies.as_array().unwrap();
    assert_eq!(hobbies_array.len(), 2);
    assert_eq!(hobbies_array[0].as_str(), Some("coding"));
    assert_eq!(hobbies_array[1].as_str(), Some("reading"));
    assert_eq!(data2_array[3].as_str(), Some("Bob")); // name: "Bob"

    client.execute("DROP TABLE e2e_json_map_roundtrip", None).await.expect("drop");
}
