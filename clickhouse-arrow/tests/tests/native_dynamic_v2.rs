use std::sync::Arc;

use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod, Result as ClickHouseResult};
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

// Helper struct for count queries
#[derive(Debug, Clone, Row)]
struct CountRow {
    count: u64,
}

/// Test Dynamic v1/v2 format with older ClickHouse servers
pub async fn test_dynamic_v2_round_trip(ch: Arc<ClickHouseContainer>) {
    let native_url = ch.get_native_url();
    debug!("ClickHouse Native URL: {native_url}");

    header("native/dynamic_v2", "Testing Dynamic v2 format");

    // Create ClientBuilder WITHOUT the v3 format setting
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&ch.user)
        .with_password(&ch.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None)
        // DO NOT set output_format_native_use_flattened_dynamic_and_json_serialization
        .build()
        .await
        .expect("Building client");

    // Check server version
    let version_check_query = "SELECT version() as version";
    let mut stream =
        client.query::<VersionRow>(version_check_query, None).await.expect("version query failed");

    let version_checker = if let Some(Ok(row)) = stream.next().await {
        let version_checker = VersionChecker::new(Some(&row.version));
        version_checker.log_compatibility_info();
        version_checker
    } else {
        warn!("Could not determine ClickHouse version");
        return;
    };

    // Only run this test for servers that support Dynamic v1/v2 but not v3
    let (major, minor) = version_checker.parsed_version();
    let supports_dynamic = match (major, minor) {
        (24, minor) if minor >= 8 => true,
        (25, minor) if minor <= 5 => true,
        _ => false,
    };

    if !supports_dynamic {
        warn!("Server version {}.{} doesn't support Dynamic v1/v2 format", major, minor);
        return;
    }

    // Create test data
    let test_data = generate_dynamic_test_block();
    let table_name = "test_dynamic_v2";

    header("dynamic_v2_test", "Setting enable_dynamic_type");
    client
        .execute("SET enable_dynamic_type = 1", None)
        .await
        .expect("set setting failed");

    header("dynamic_v2_test", "Creating table with Dynamic column");
    client
        .execute(&format!("DROP TABLE IF EXISTS {table_name}"), None)
        .await
        .expect("drop table failed");

    client
        .execute(&format!("CREATE TABLE {table_name} (dynamic_col Dynamic) ENGINE = Memory"), None)
        .await
        .expect("create table failed");

    header("dynamic_v2_test", "Inserting Dynamic data");
    let insert_query = format!("INSERT INTO {table_name} VALUES");
    let mut stream = client.insert(&insert_query, test_data, None).await.expect("insert failed");

    while let Some(result) = stream.next().await {
        result.expect("insert stream failed");
    }

    header("dynamic_v2_test", "Checking row count");
    let count_query = format!("SELECT count() as count FROM {table_name}");
    let mut count_stream =
        client.query::<CountRow>(&count_query, None).await.expect("count query failed");

    if let Some(Ok(row)) = count_stream.next().await {
        assert_eq!(row.count, 3, "Expected 3 rows in table");
    }

    header("dynamic_v2_test", "Querying Dynamic data");
    let query = format!("SELECT * FROM {table_name}");
    let mut stream = client.query::<DynamicRow>(&query, None).await.expect("query failed");

    let mut received_values = Vec::new();
    while let Some(Ok(row)) = stream.next().await {
        received_values.push(row.dynamic_col);
    }

    header("dynamic_v2_test", "Verifying Dynamic data");
    let expected_values = &generate_dynamic_test_block().column_data;
    assert_eq!(received_values.len(), expected_values.len(), "Row count mismatch");

    // Verify each value
    for (i, (expected, received)) in expected_values.iter().zip(received_values.iter()).enumerate()
    {
        assert_eq!(expected, received, "Value mismatch at index {i}");
    }

    header("dynamic_v2_test", format!("Dropping table {table_name}"));
    client
        .execute(&format!("DROP TABLE {table_name}"), None)
        .await
        .expect("drop table failed");

    header("dynamic_v2_test", "Dynamic v2 test completed successfully");
}