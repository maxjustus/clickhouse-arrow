use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod, CreateOptions, Result as ClickHouseResult};
use futures_util::StreamExt;
use tracing::{debug, warn};

use crate::common::header;
use crate::common::native_helpers::*;
use crate::common::test_helpers::*;

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

    // Create ClientBuilder and ConnectionManager
    let mut builder = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None);

    // Only use v3 format setting for servers that support it (25.6+)
    let version_str = std::env::var("CLICKHOUSE_VERSION").ok();
    let should_use_v3 = match version_str.as_deref() {
        Some(v) if v.starts_with("24.") => false,
        Some(v) if v.starts_with("25.") => {
            let parts: Vec<&str> = v.split('.').collect();
            if parts.len() >= 2 { parts[1].parse::<u32>().unwrap_or(0) >= 6 } else { false }
        }
        _ => true, // Default to v3 for latest
    };

    if should_use_v3 {
        builder = builder
            .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1);
    }

    let client: NativeClient = builder.build().await.expect("Building client");

    // Check if the server supports Dynamic type
    let _version_checker = match check_version_support(&client, "Dynamic type test", true, false).await {
        Some(checker) => checker,
        None => return,
    };
    
    // Log the actual format we'll be using
    debug!("Using Dynamic format version: {}", if should_use_v3 { "v3" } else { "v1/v2" });

    // Test Dynamic type with direct block operations
    let test_data = generate_dynamic_test_block();

    // Create a test table with Dynamic column
    let query_id = "dynamic_test";
    let table_name = "test_dynamic";

    header(query_id, "Setting enable_dynamic_type globally");
    client.execute("SET enable_dynamic_type = 1", None).await.expect("set setting failed");

    create_test_table(&client, table_name, "dynamic_col Dynamic", query_id).await.expect("table creation failed");

    debug!("Test data: {} rows, column types: {:?}", test_data.rows, test_data.column_types);
    insert_test_data(&client, table_name, test_data, query_id).await.expect("insert failed");

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
    let simple_query = "SELECT 1 as num";
    let mut simple_stream =
        client.query::<SimpleRow>(simple_query, None).await.expect("simple query failed");

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
        assert_eq!(expected, received, "Value mismatch at index {i}");
    }

    drop_test_table(&client, table_name, query_id).await.expect("drop table failed");

    header(query_id, "Dynamic type test completed successfully");
}

/// # Panics
pub async fn test_json_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    header("native/json", "Testing JSON type round trip");

    // Table create options
    let _options = CreateOptions::new("MergeTree");

    // Create ClientBuilder and ConnectionManager
    let mut builder = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None);

    // Only use v3 format setting for servers that support it (25.6+)
    let version_str = std::env::var("CLICKHOUSE_VERSION").ok();
    let should_use_v3 = match version_str.as_deref() {
        Some(v) if v.starts_with("24.") => false,
        Some(v) if v.starts_with("25.") => {
            let parts: Vec<&str> = v.split('.').collect();
            if parts.len() >= 2 { parts[1].parse::<u32>().unwrap_or(0) >= 6 } else { false }
        }
        _ => true, // Default to v3 for latest
    };

    if should_use_v3 {
        builder = builder
            .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1);
    }

    let client: NativeClient = builder.build().await.expect("Building client");

    // Check if the server supports JSON type
    let _version_checker = match check_version_support(&client, "JSON type test", false, true).await {
        Some(checker) => checker,
        None => return,
    };

    // Test JSON type with direct block operations
    let test_data = generate_json_test_block();

    // Create a test table with JSON column
    let query_id = "json_test";
    let table_name = "test_json";

    create_test_table(&client, table_name, "json_col JSON", query_id).await.expect("table creation failed");

    insert_test_data(&client, table_name, test_data, query_id).await.expect("insert failed");

    header(query_id, "Checking row count");
    let count_query = format!("SELECT count() FROM {table_name}");
    let mut count_stream =
        client.query::<CountRow>(&count_query, None).await.expect("count query failed");

    if let Some(Ok(row)) = count_stream.next().await {
        assert!(row.count > 0, "Expected rows in table");
    }

    header(query_id, "Testing simple query first");
    let simple_query = "SELECT 1 as num";
    let mut simple_stream =
        client.query::<SimpleRow>(simple_query, None).await.expect("simple query failed");

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

        assert_eq!(expected_json, received_json, "JSON value mismatch at index {i}");
    }

    drop_test_table(&client, table_name, query_id).await.expect("drop table failed");

    header(query_id, "JSON type test completed successfully");
}

// Helper struct for mixed Dynamic and JSON columns
#[derive(Debug, Clone, Row)]
struct MixedRow {
    dynamic_col: Value,
    json_col:    Value,
}

/// Test mixed Dynamic and JSON columns in the same insert to verify per-column state handling
/// # Panics
pub async fn test_mixed_dynamic_json(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    header("native/mixed", "Testing mixed Dynamic and JSON columns");

    // Create ClientBuilder and ConnectionManager
    let mut builder = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None);

    // Only use v3 format setting for servers that support it (25.6+)
    let version_str = std::env::var("CLICKHOUSE_VERSION").ok();
    let should_use_v3 = match version_str.as_deref() {
        Some(v) if v.starts_with("24.") => false,
        Some(v) if v.starts_with("25.") => {
            let parts: Vec<&str> = v.split('.').collect();
            if parts.len() >= 2 { parts[1].parse::<u32>().unwrap_or(0) >= 6 } else { false }
        }
        _ => true, // Default to v3 for latest
    };

    if should_use_v3 {
        builder = builder
            .with_setting("output_format_native_use_flattened_dynamic_and_json_serialization", 1);
    }

    let client: NativeClient = builder.build().await.expect("Building client");

    // Check if the server supports both Dynamic and JSON types
    let _version_checker = match check_version_support(&client, "Mixed Dynamic/JSON test", true, true).await {
        Some(checker) => checker,
        None => return,
    };

    // Generate test data with both Dynamic and JSON columns
    let dynamic_test_data = generate_dynamic_test_block().column_data;
    let json_test_data = generate_json_test_block().column_data;

    // Create mixed test block with both columns
    let mixed_test_block = generate_mixed_dynamic_json_test_block();

    let query_id = "mixed_test";
    let table_name = "test_mixed_dynamic_json";

    header(query_id, "Setting enable_dynamic_type globally");
    client.execute("SET enable_dynamic_type = 1", None).await.expect("set setting failed");

    create_test_table(&client, table_name, "dynamic_col Dynamic, json_col JSON", query_id).await.expect("table creation failed");

    debug!(
        "Test data: {} rows, column types: {:?}",
        mixed_test_block.rows, mixed_test_block.column_types
    );
    insert_test_data(&client, table_name, mixed_test_block, query_id).await.expect("insert failed");

    header(query_id, "Checking row count");
    let count_query = format!("SELECT count() FROM {table_name}");
    let mut count_stream =
        client.query::<CountRow>(&count_query, None).await.expect("count query failed");

    if let Some(Ok(row)) = count_stream.next().await {
        assert!(row.count > 0, "Expected rows in table");
    }

    header(query_id, "Querying mixed Dynamic and JSON data");
    let query = format!("SELECT dynamic_col, json_col FROM {table_name}");
    let mut stream = client.query::<MixedRow>(&query, None).await.expect("query failed");

    let mut received_rows = Vec::new();
    while let Some(Ok(row)) = stream.next().await {
        received_rows.push(row);
    }

    header(query_id, "Verifying mixed data");
    assert_eq!(received_rows.len(), dynamic_test_data.len(), "Row count mismatch");

    // Verify each row
    for (i, received_row) in received_rows.iter().enumerate() {
        // Check Dynamic column
        let expected_dynamic = &dynamic_test_data[i];
        assert_eq!(
            expected_dynamic, &received_row.dynamic_col,
            "Dynamic value mismatch at index {i}"
        );

        // Check JSON column
        let expected_json = &json_test_data[i];

        // Extract JSON strings from Value::String for comparison
        let expected_str = match expected_json {
            Value::String(bytes) => String::from_utf8(bytes.clone()).expect("Valid UTF-8"),
            _ => panic!("Expected Value::String for expected JSON"),
        };

        let received_str = match &received_row.json_col {
            Value::String(bytes) => String::from_utf8(bytes.clone()).expect("Valid UTF-8"),
            _ => panic!("Expected Value::String for received JSON"),
        };

        // Parse both as JSON to compare semantically
        let expected_json_value: serde_json::Value =
            serde_json::from_str(&expected_str).expect("Expected value should be valid JSON");
        let received_json_value: serde_json::Value =
            serde_json::from_str(&received_str).expect("Received value should be valid JSON");

        assert_eq!(expected_json_value, received_json_value, "JSON value mismatch at index {i}");
    }

    drop_test_table(&client, table_name, query_id).await.expect("drop table failed");

    header(
        query_id,
        "Mixed Dynamic/JSON test completed successfully - this confirms per-column state works!",
    );
}
