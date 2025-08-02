use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod, CreateOptions, Result as ClickHouseResult};
use futures_util::StreamExt;
use tracing::debug;

use crate::common::header;
use crate::common::native_helpers::*;

// Helper struct for Dynamic queries
#[derive(Debug, Clone, Row)]
struct DynamicRow {
    dynamic_col: Value,
}

// Helper struct for JSON queries
#[derive(Debug, Clone, Row)]
struct JsonRow {
    json_col: Value,
}

// Helper struct for count queries
#[derive(Debug, Clone, Row)]
struct CountRow {
    count: u64,
}

// Helper struct for type check queries
#[derive(Debug, Clone, Row)]
#[allow(dead_code)]
struct TypeCheckRow {
    dtype: String,
}

// Helper struct for simple queries
#[derive(Debug, Clone, Row)]
struct SimpleRow {
    num: u8,
}

// Helper functions to reduce repetitive patterns

/// Check if server supports v3 format (`ClickHouse` 25.6+)
fn should_use_v3_format() -> bool {
    let version_str = std::env::var("CLICKHOUSE_VERSION").ok();
    match version_str.as_deref() {
        Some(v) if v.starts_with("24.") => false,
        Some(v) if v.starts_with("25.") => {
            let parts: Vec<&str> = v.split('.').collect();
            if parts.len() >= 2 { parts[1].parse::<u32>().unwrap_or(0) >= 6 } else { false }
        }
        _ => true, // Default to v3 for latest
    }
}

/// Create a basic native client with standard settings
async fn create_basic_native_client(
    ch: &ClickHouseContainer,
    compression: CompressionMethod,
) -> NativeClient {
    ClientBuilder::new()
        .with_endpoint(ch.get_native_url())
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(compression)
        .build()
        .await
        .expect("Building client")
}

/// Create a native client with v3 format support for Dynamic/JSON types
async fn create_v3_native_client(ch: &ClickHouseContainer) -> NativeClient {
    let mut builder = ClientBuilder::new()
        .with_endpoint(ch.get_native_url())
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None);

    if should_use_v3_format() {
        builder = builder
            .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1);
    }

    builder.build().await.expect("Building client")
}

/// # Panics
pub async fn test_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    let options = CreateOptions::new("MergeTree").with_order_by(&["id".to_string()]);

    let client = create_basic_native_client(&ch, CompressionMethod::LZ4).await;
    let test_data = generate_test_block();
    round_trip(client, test_data, &options)
        .await
        .inspect_err(|error| {
            error!("Round trip for Native failed: {error:?}");
        })
        .expect("Round trip failed");
}

/// # Errors
/// # Panics
pub async fn round_trip<T: Row + std::fmt::Debug + PartialEq + Clone + Send + Sync + 'static>(
    client: NativeClient,
    data: Vec<T>,
    options: &CreateOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    // Generate unique database and table names
    let table_qid = Qid::new();

    let db_name = format!("test_db_{table_qid}");
    let table_name = format!("test_table_{table_qid}");

    // Drop table
    let query_id = Qid::new();
    header(query_id, format!("Dropping table: {db_name}.{table_name}"));
    client
        .execute(format!("DROP TABLE IF EXISTS {db_name}.{table_name}"), Some(table_qid))
        .await?;

    // Drop database
    let query_id = Qid::new();
    header(query_id, format!("Dropping database: {db_name}"));
    client.execute(format!("DROP DATABASE IF EXISTS {db_name}"), Some(table_qid)).await?;

    // Create database
    let query_id = Qid::new();
    header(query_id, format!("Creating database: {db_name}"));
    client
        .execute(format!("CREATE DATABASE IF NOT EXISTS {db_name}"), Some(table_qid))
        .await?;

    // Create table
    let query_id = Qid::new();
    header(query_id, format!("Creating table: {db_name}.{table_name}"));
    client.create_table::<T>(Some(&db_name), &table_name, options, Some(table_qid)).await?;

    // Insert data
    let query_id = Qid::new();
    let data_len = data.len();
    header(query_id, format!("Inserting test data with {data_len} rows"));
    let query = format!("INSERT INTO {db_name}.{table_name} FORMAT Native");
    let result = client
        .insert_rows(&query, data.clone().into_iter(), Some(table_qid))
        .await
        .inspect_err(|error| error!(?error, "Insertion failed: {query_id}"))?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<ClickHouseResult<Vec<_>>>()
        .inspect_err(|error| error!(?error, "Failed to insert rows: {query_id}"))?;
    drop(result);

    // Sleep wait for data
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Query and verify results
    let query_id = Qid::new();
    header(query_id, format!("Querying table: {db_name}.{table_name}"));
    let query = format!("SELECT * FROM {db_name}.{table_name}");
    let queried_rows = client
        .query(&query, Some(table_qid))
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<ClickHouseResult<Vec<T>>>()?;

    // Verify queried data matches inserted data
    header(query_id, "Verifying queried data");
    let inserted_rows = data;

    assert_eq!(queried_rows.len(), inserted_rows.len(), "Expected equal rows");
    assert_eq!(queried_rows, inserted_rows, "Expected round trip of data");

    // Truncate table
    header(query_id, format!("Truncating table: {db_name}.{table_name}"));
    client.execute(format!("TRUNCATE TABLE {db_name}.{table_name}"), Some(table_qid)).await?;

    // Drop table
    header(query_id, format!("Dropping table: {db_name}.{table_name}"));
    client.execute(format!("DROP TABLE {db_name}.{table_name}"), None).await?;

    // Drop database
    header(query_id, format!("Dropping database: {db_name}"));
    client.execute(format!("DROP DATABASE {db_name}"), None).await?;

    header(query_id, "Round-trip test completed successfully");

    Ok(())
}

/// # Panics
pub async fn test_variant_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    let options = CreateOptions::new("MergeTree").with_order_by(&["id".to_string()]);

    let client = create_basic_native_client(&ch, CompressionMethod::LZ4).await;

    let test_data = generate_variant_test_block();
    round_trip(client, test_data, &options)
        .await
        .inspect_err(|error| {
            error!("Round trip for Variant Native failed: {error:?}");
        })
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
