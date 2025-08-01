use clickhouse_arrow::prelude::*;
use clickhouse_arrow::native::block::Block;
use futures_util::StreamExt;
use tracing::warn;

use crate::common::header;
use crate::common::version_compat::VersionChecker;

/// Generic helper struct for single-field queries
#[derive(Debug, Clone, Row)]
pub struct VersionRow {
    pub version: String,
}

/// Helper function to create a test table with DROP IF EXISTS + CREATE TABLE
pub async fn create_test_table(client: &NativeClient, table_name: &str, column_spec: &str, query_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    header(query_id, &format!("Creating table {table_name}"));
    
    // Drop table if exists
    client
        .execute(&format!("DROP TABLE IF EXISTS {table_name}"), None)
        .await
        .expect("drop table failed");

    // Create table
    client
        .execute(&format!("CREATE TABLE {table_name} ({column_spec}) ENGINE = Memory"), None)
        .await
        .expect("create table failed");
        
    Ok(())
}

/// Helper function to insert test data into a table
pub async fn insert_test_data(client: &NativeClient, table_name: &str, data: Block, query_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    header(query_id, "Inserting test data");
    let insert_query = format!("INSERT INTO {table_name} VALUES");
    let mut stream = client.insert(&insert_query, data, None).await.expect("insert failed");

    while let Some(result) = stream.next().await {
        result.expect("insert stream failed");
    }
    Ok(())
}

/// Helper function to drop a test table with logging
pub async fn drop_test_table(client: &NativeClient, table_name: &str, query_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    header(query_id, &format!("Dropping table {table_name}"));
    client
        .execute(&format!("DROP TABLE {table_name}"), None)
        .await
        .expect("drop table failed");
    Ok(())
}

/// Helper function to check version and return VersionChecker, or return early if unsupported
pub async fn check_version_support(client: &NativeClient, test_name: &str, needs_dynamic: bool, needs_json: bool) -> Option<VersionChecker> {
    let version_check_query = "SELECT version() as version";
    let mut stream = client.query::<VersionRow>(version_check_query, None).await.expect("version query failed");
    
    if let Some(Ok(row)) = stream.next().await {
        let version_checker = VersionChecker::new(Some(&row.version));
        version_checker.log_compatibility_info();
        
        if needs_dynamic && !version_checker.require_dynamic_support(test_name) {
            return None;
        }
        if needs_json && !version_checker.require_json_support(test_name) {
            return None;
        }
        Some(version_checker)
    } else {
        warn!("Could not determine ClickHouse version, skipping {}", test_name);
        None
    }
}