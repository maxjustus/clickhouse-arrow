use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use clickhouse_arrow::{
    Client, ClientBuilder, CompressionMethod, NativeFormat, Qid, Value as ChValue,
};
use futures::StreamExt;
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

    /// Execute a query and return results as JSON
    pub async fn execute_query(
        &self,
        query: &str,
        _settings: HashMap<String, Value>,
        _params: HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        tracing::debug!("Executing query: {}", query);

        let mut stream = self
            .client
            .query_raw(query.to_string(), None::<HashMap<String, String>>, Qid::default())
            .await
            .context("Failed to execute query")?;

        let mut results = Vec::new();
        while let Some(block_result) = stream.next().await {
            let block = block_result.context("Failed to read block")?;

            let json_result = block_to_json(block)?;

            results.push(json_result);
        }

        Ok(results)
    }

    /// Insert batch data into a table
    pub async fn insert_batch(&self, table: &str, data: Vec<Value>) -> Result<()> {
        tracing::debug!("Inserting {} rows into table: {}", data.len(), table);

        // For now, we'll use a simple approach - convert JSON to VALUES format
        // In a real implementation, we'd want to use the native insert capabilities
        if data.is_empty() {
            return Ok(());
        }

        // Extract column names from the first row
        let first_row =
            data.first().and_then(|v| v.as_object()).context("First row must be a JSON object")?;

        let columns: Vec<String> = first_row.keys().cloned().collect();
        let columns_str = columns.join(", ");

        // Convert each row to VALUES format
        let mut values_parts = Vec::new();
        for row in &data {
            let row_obj = row.as_object().context("Each row must be a JSON object")?;

            let mut row_values = Vec::new();
            for column in &columns {
                let value = row_obj.get(column).unwrap_or(&Value::Null);
                row_values.push(json_to_clickhouse_literal(value)?);
            }
            values_parts.push(format!("({})", row_values.join(", ")));
        }

        let values_str = values_parts.join(", ");
        let insert_query = format!("INSERT INTO {table} ({columns_str}) VALUES {values_str}");

        // Execute the insert
        let mut stream = self
            .client
            .query_raw(insert_query, None::<HashMap<String, String>>, Qid::default())
            .await
            .context("Failed to execute insert")?;

        // Consume the stream to complete the insert
        while let Some(block_result) = stream.next().await {
            if let Ok(_block) = block_result {
                // Block processed
            }
        }

        Ok(())
    }

    /// Get server information
    pub async fn get_server_info(&self) -> Result<Value> {
        let query = "SELECT version(), uptime()";

        let mut stream = self
            .client
            .query_raw(query.to_string(), None::<HashMap<String, String>>, Qid::default())
            .await
            .context("Failed to get server info")?;

        if let Some(block_result) = stream.next().await {
            let block = block_result.context("Failed to read server info block")?;

            let json_result = block_to_json(block)?;

            return Ok(json_result);
        }

        Ok(Value::Null)
    }
}

/// Convert Block to JSON array - one object per row with proper column names
fn block_to_json(block: clickhouse_arrow::native::block::Block) -> Result<Value> {
    let mut result_rows = Vec::new();
    let rows = block.rows as usize;

    if rows == 0 {
        return Ok(Value::Array(vec![]));
    }

    // Extract column names and types
    let column_info: Vec<_> = block.column_types.iter().collect();

    // Convert columnar data to row-based JSON
    // The block.column_data contains all values flattened:
    // for each column, it has `rows` consecutive values
    for row_idx in 0..rows {
        let mut json_row = serde_json::Map::new();

        for (column_name, _column_type) in column_info.iter() {
            // Get the value for this column and row
            let column_start =
                column_info.iter().position(|(name, _)| name == column_name).unwrap() * rows;
            let value_idx = column_start + row_idx;

            if value_idx < block.column_data.len() {
                let value = clickhouse_value_to_json(block.column_data[value_idx].clone())?;
                json_row.insert(column_name.clone(), value);
            } else {
                tracing::warn!("Missing data for column {} row {}", column_name, row_idx);
                json_row.insert(column_name.clone(), Value::Null);
            }
        }

        result_rows.push(Value::Object(json_row));
    }

    // If there's only one row, return it directly instead of an array
    if result_rows.len() == 1 {
        Ok(result_rows.into_iter().next().unwrap())
    } else {
        Ok(Value::Array(result_rows))
    }
}

/// Convert ClickHouse Value to JSON
fn clickhouse_value_to_json(value: ChValue) -> Result<Value> {
    // Use the built-in to_json method from clickhouse-arrow
    value.to_json().map_err(|e| anyhow!("Failed to convert to JSON: {}", e))
}

/// Convert JSON value to ClickHouse literal string for INSERT statements
fn json_to_clickhouse_literal(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("NULL".to_string()),
        Value::Bool(b) => Ok(if *b { "1".to_string() } else { "0".to_string() }),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(format!("'{}'", s.replace("'", "\\'"))),
        Value::Array(arr) => {
            let elements: Result<Vec<String>> =
                arr.iter().map(json_to_clickhouse_literal).collect();
            Ok(format!("[{}]", elements?.join(", ")))
        }
        Value::Object(_) => {
            // For complex objects, convert to JSON string
            Ok(format!("'{}'", serde_json::to_string(value)?.replace("'", "\\'")))
        }
    }
}

/// Simple base64 encoding helper
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();

    for chunk in data.chunks(3) {
        let mut buf = [0u8; 3];
        for (i, &byte) in chunk.iter().enumerate() {
            buf[i] = byte;
        }

        let b = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32;

        result.push(ALPHABET[((b >> 18) & 63) as usize] as char);
        result.push(ALPHABET[((b >> 12) & 63) as usize] as char);
        result.push(if chunk.len() > 1 { ALPHABET[((b >> 6) & 63) as usize] as char } else { '=' });
        result.push(if chunk.len() > 2 { ALPHABET[(b & 63) as usize] as char } else { '=' });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_to_clickhouse_literal() {
        assert_eq!(json_to_clickhouse_literal(&Value::Null).unwrap(), "NULL");
        assert_eq!(json_to_clickhouse_literal(&Value::Bool(true)).unwrap(), "1");
        assert_eq!(json_to_clickhouse_literal(&Value::Bool(false)).unwrap(), "0");
        assert_eq!(json_to_clickhouse_literal(&Value::Number(42.into())).unwrap(), "42");
        assert_eq!(
            json_to_clickhouse_literal(&Value::String("test".to_string())).unwrap(),
            "'test'"
        );
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
