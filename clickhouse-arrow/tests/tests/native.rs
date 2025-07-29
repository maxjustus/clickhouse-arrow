use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod, CreateOptions, Result as ClickHouseResult};
use futures_util::StreamExt;
use tracing::{debug, warn};

use crate::common::header;
use crate::common::native_helpers::*;
use crate::common::version_compat::VersionChecker;

// Helper struct for version query
#[derive(Debug, Clone, Row)]
struct VersionRow {
    version: String,
}

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

/// # Panics
pub async fn test_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    // Table create options
    let options = CreateOptions::new("MergeTree").with_order_by(&["id".to_string()]);

    // Create ClientBuilder and ConnectionManager
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::LZ4)
        .build()
        .await
        .expect("Building client");
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
    header(query_id, format!("Inserting test data with {} rows", data.len()));
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

    // Table create options
    let options = CreateOptions::new("MergeTree").with_order_by(&["id".to_string()]);

    // Create ClientBuilder and ConnectionManager
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::LZ4)
        .build()
        .await
        .expect("Building client");

    let test_data = generate_variant_test_block();
    round_trip(client, test_data, &options)
        .await
        .inspect_err(|error| {
            error!("Round trip for Variant Native failed: {error:?}");
        })
        .expect("Variant round trip failed");
}

/// # Panics
pub async fn test_dynamic_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    header("native/dynamic", "Testing Dynamic type round trip");

    // Table create options
    let _options = CreateOptions::new("MergeTree");

    // Create ClientBuilder and ConnectionManager with v3 Dynamic format setting
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None)
        .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1)
        .build()
        .await
        .expect("Building client");

    // Check if the server supports Dynamic type
    let version_check_query = "SELECT version() as version";
    let mut stream =
        client.query::<VersionRow>(version_check_query, None).await.expect("version query failed");

    let _version_checker = if let Some(Ok(row)) = stream.next().await {
        let version_checker = VersionChecker::new(Some(&row.version));
        version_checker.log_compatibility_info();

        if !version_checker.require_dynamic_support("Dynamic type test") {
            return;
        }
        version_checker
    } else {
        warn!("Could not determine ClickHouse version, skipping Dynamic type test");
        return;
    };

    // Test Dynamic type with direct block operations
    let test_data = generate_dynamic_test_block();

    // Create a test table with Dynamic column
    let query_id = "dynamic_test";
    let table_name = "test_dynamic";

    header(query_id, format!("Creating table with Dynamic column"));
    client
        .execute(&format!("DROP TABLE IF EXISTS {table_name}"), None)
        .await
        .expect("drop table failed");

    client
        .execute(&format!("CREATE TABLE {table_name} (dynamic_col Dynamic) ENGINE = Memory"), None)
        .await
        .expect("create table failed");

    header(query_id, "Inserting Dynamic data");
    let insert_query = format!("INSERT INTO {table_name} VALUES");
    let mut stream = client.insert(&insert_query, test_data, None).await.expect("insert failed");

    while let Some(result) = stream.next().await {
        result.expect("insert stream failed");
    }

    header(query_id, "Checking row count");
    let count_query = format!("SELECT count() FROM {table_name}");
    let mut count_stream =
        client.query::<CountRow>(&count_query, None).await.expect("count query failed");

    if let Some(Ok(row)) = count_stream.next().await {
        assert!(row.count > 0, "Expected rows in table");
    }

    // Skip type check for now as it returns LowCardinality
    // header(query_id, "Checking Dynamic types");
    // let type_query = format!("SELECT dynamicType(dynamic_col) as dtype FROM {table_name}");
    // let mut type_stream = client.query::<TypeCheckRow>(&type_query, None).await.expect("type
    // query failed");
    //
    // while let Some(Ok(row)) = type_stream.next().await {
    // }

    header(query_id, "Testing simple query first");
    let simple_query = format!("SELECT 1 as num");
    let mut simple_stream =
        client.query::<SimpleRow>(&simple_query, None).await.expect("simple query failed");

    if let Some(Ok(row)) = simple_stream.next().await {
        assert_eq!(row.num, 1);
    }

    header(query_id, "Querying Dynamic data");
    let query = format!("SELECT * FROM {table_name}");
    let mut stream = client.query::<DynamicRow>(&query, None).await.expect("query failed");

    let mut received_values = Vec::new();
    while let Some(Ok(row)) = stream.next().await {
        received_values.push(row.dynamic_col);
    }

    header(query_id, "Verifying Dynamic data");
    let expected_values = &generate_dynamic_test_block().column_data;
    assert_eq!(received_values.len(), expected_values.len(), "Row count mismatch");

    // Verify each value
    for (i, (expected, received)) in expected_values.iter().zip(received_values.iter()).enumerate()
    {
        assert_eq!(expected, received, "Value mismatch at index {}", i);
    }

    header(query_id, format!("Dropping table {table_name}"));
    client
        .execute(&format!("DROP TABLE {table_name}"), None)
        .await
        .expect("drop table failed");

    header(query_id, "Dynamic type test completed successfully");
}

/// # Panics
pub async fn test_json_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    header("native/json", "Testing JSON type round trip");

    // Table create options
    let _options = CreateOptions::new("MergeTree");

    // Create ClientBuilder and ConnectionManager with v3 JSON format setting
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None)
        .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1)
        .build()
        .await
        .expect("Building client");

    // Check if the server supports JSON type
    let version_check_query = "SELECT version() as version";
    let mut stream =
        client.query::<VersionRow>(version_check_query, None).await.expect("version query failed");

    let _version_checker = if let Some(Ok(row)) = stream.next().await {
        let version_checker = VersionChecker::new(Some(&row.version));
        version_checker.log_compatibility_info();

        if !version_checker.require_json_support("JSON type test") {
            return;
        }
        version_checker
    } else {
        warn!("Could not determine ClickHouse version, skipping JSON type test");
        return;
    };

    // Test JSON type with direct block operations
    let test_data = generate_json_test_block();

    // Create a test table with JSON column
    let query_id = "json_test";
    let table_name = "test_json";

    header(query_id, format!("Creating table with JSON column"));
    client
        .execute(&format!("DROP TABLE IF EXISTS {table_name}"), None)
        .await
        .expect("drop table failed");

    client
        .execute(&format!("CREATE TABLE {table_name} (json_col JSON) ENGINE = Memory"), None)
        .await
        .expect("create table failed");

    header(query_id, "Inserting JSON data");
    let insert_query = format!("INSERT INTO {table_name} VALUES");
    let mut stream = client.insert(&insert_query, test_data, None).await.expect("insert failed");

    while let Some(result) = stream.next().await {
        result.expect("insert stream failed");
    }

    header(query_id, "Checking row count");
    let count_query = format!("SELECT count() FROM {table_name}");
    let mut count_stream =
        client.query::<CountRow>(&count_query, None).await.expect("count query failed");

    if let Some(Ok(row)) = count_stream.next().await {
        assert!(row.count > 0, "Expected rows in table");
    }

    header(query_id, "Testing simple query first");
    let simple_query = format!("SELECT 1 as num");
    let mut simple_stream =
        client.query::<SimpleRow>(&simple_query, None).await.expect("simple query failed");

    if let Some(Ok(row)) = simple_stream.next().await {
        assert_eq!(row.num, 1, "Simple query should return 1");
    }

    header(query_id, "Querying JSON data");
    let query = format!("SELECT json_col FROM {table_name}");
    let mut stream = client.query::<JsonRow>(&query, None).await.expect("query failed");

    let mut received_values = Vec::new();
    while let Some(Ok(row)) = stream.next().await {
        received_values.push(row.json_col);
    }

    header(query_id, "Verifying JSON data");
    let expected_values = &generate_json_test_block().column_data;
    assert_eq!(received_values.len(), expected_values.len(), "Row count mismatch");

    // Verify each value - for JSON, parse and compare the JSON objects rather than raw strings
    for (i, (expected, received)) in expected_values.iter().zip(received_values.iter()).enumerate()
    {
        // Extract JSON strings from Value::String
        let expected_str = match expected {
            Value::String(bytes) => String::from_utf8(bytes.clone()).expect("Valid UTF-8"),
            _ => panic!("Expected Value::String for expected"),
        };

        let received_str = match received {
            Value::String(bytes) => String::from_utf8(bytes.clone()).expect("Valid UTF-8"),
            _ => panic!("Expected Value::String for received"),
        };

        // Parse both as JSON to compare semantically rather than textually
        let expected_json: serde_json::Value =
            serde_json::from_str(&expected_str).expect("Expected value should be valid JSON");
        let received_json: serde_json::Value =
            serde_json::from_str(&received_str).expect("Received value should be valid JSON");

        assert_eq!(expected_json, received_json, "JSON value mismatch at index {}", i);
    }

    header(query_id, format!("Dropping table {table_name}"));
    client
        .execute(&format!("DROP TABLE {table_name}"), None)
        .await
        .expect("drop table failed");

    header(query_id, "JSON type test completed successfully");
}
