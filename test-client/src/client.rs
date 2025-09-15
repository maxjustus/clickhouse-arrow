use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use clickhouse_arrow::{Client, ClientBuilder, CompressionMethod, InsertOptions, NativeFormat};
use serde_json::Value;

/// ClickHouse client wrapper for testing the native format implementation
pub struct ClickHouseClient {
    client: Client<NativeFormat>,
}

impl ClickHouseClient {
    /// Create a new ClickHouse client
    pub async fn new(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
        secure: bool,
        compression: &str,
    ) -> Result<Self> {
        let endpoint =
            if secure { format!("https://{host}:{port}") } else { format!("{host}:{port}") };

        let mut builder = ClientBuilder::new()
            .with_endpoint(&endpoint)
            .with_username(user)
            .with_password(password)
            .with_database(database);

        // Configure compression
        match compression {
            "lz4" => builder = builder.with_compression(CompressionMethod::LZ4),
            "zstd" => builder = builder.with_compression(CompressionMethod::ZSTD),
            "none" => builder = builder.with_compression(CompressionMethod::None),
            _ => return Err(anyhow::anyhow!("Unsupported compression: {}", compression)),
        }

        let client = builder.build_native().await.context("Failed to build ClickHouse client")?;

        Ok(Self { client })
    }

    pub fn native_client(&self) -> &Client<NativeFormat> { &self.client }

    /// Execute a query and return results as Serde JSON
    pub async fn execute_query(
        &self,
        query: &str,
        _settings: HashMap<String, Value>,
        _params: HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        tracing::debug!("Executing query: {}", query);

        // Use high-level JSON query API to get serde_json::Value rows directly
        let rows = self
            .client
            .query_json(query.to_string(), None)
            .await
            .context("Failed to execute query")?;

        // Convert Vec<Map<..>> into Vec<Value::Object>
        Ok(rows.into_iter().map(Value::Object).collect())
    }

    /// Insert batch data into a table
    pub async fn insert_batch(
        &self,
        table: &str,
        data: Vec<Value>,
        columns: Option<Vec<String>>,
    ) -> Result<()> {
        tracing::debug!("Inserting {} rows into table: {}", data.len(), table);
        if data.is_empty() {
            return Ok(());
        }

        // Use header-driven InsertInto with serde rows directly
        let mut opts = InsertOptions::default();
        opts.columns = columns;
        let mut op = self
            .client
            .insert_into(table, opts)
            .await
            .context("Failed to open insert operation")?;
        // Ensure each row is an object and write in one batch
        if let Some(bad) = data.iter().find(|v| !v.is_object()) {
            return Err(anyhow!("Each row must be a JSON object; found: {}", bad));
        }
        op.write_rows(data).await.context("Failed to write rows")?;
        op.finish().await.context("Failed to finish insert")
    }

    /// Get server information
    pub async fn get_server_info(&self) -> Result<Value> {
        let rows = self
            .client
            .query_json("SELECT version(), uptime()", None)
            .await
            .context("Failed to get server info")?;
        Ok(rows.into_iter().next().map(Value::Object).unwrap_or(Value::Null))
    }
}
