#![allow(unused_extern_crates)]
#![cfg(feature = "test-utils")]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clickhouse_arrow::native::block::Block;
    use clickhouse_arrow::native::block_info::BlockInfo;
    use clickhouse_arrow::native::types::Type;
    use clickhouse_arrow::native::values::Value;
    use clickhouse_arrow::prelude::*;
    use clickhouse_arrow::test_utils::*;

    // Verify that sending more dynamic types than max_dynamic_types does not fail client-side
    // and that server still accepts and reconstructs JSON correctly.
    #[tokio::test]
    async fn test_json_max_dynamic_types_server_selection() -> Result<(), Box<dyn std::error::Error>>
    {
        // Start shared container
        let container: Arc<ClickHouseContainer> = get_shared_container().await;

        // Create client
        let client = ClientBuilder::default()
            .with_endpoint(container.get_native_url())
            .with_username("clickhouse")
            .with_password("clickhouse")
            .build::<NativeFormat>()
            .await?;

        // Prepare table: limit dynamic types to 2 on the server-side schema
        client.execute("DROP TABLE IF EXISTS e2e_json_max_dyn_types", None).await?;
        client
            .execute(
                r#"
        CREATE TABLE e2e_json_max_dyn_types (
            data JSON(max_dynamic_types=2)
        ) ENGINE = MergeTree() ORDER BY tuple()
        "#,
                None,
            )
            .await?;

        // Build rows where single dynamic path 'value' appears with > 2 distinct types
        let rows = vec![
            Value::String(br#"{"value": "str", "other": 1}"#.to_vec()), // String
            Value::String(br#"{"value": 42}"#.to_vec()),                // Int64
            Value::String(br#"{"value": 3.14159}"#.to_vec()),           // Float64
            Value::String(br#"{"value": [1, 2, 3]}"#.to_vec()),         // Array(Int64)
        ];

        let type_ = Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: Some(2),
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        };

        let block = Block {
            info:         BlockInfo::default(),
            rows:         rows.len() as u64,
            column_types: vec![("data".to_string(), type_)],
            column_data:  rows.clone(),
        };

        // Insert using native without client-side capping; should succeed
        use futures_util::StreamExt;
        let mut stream =
            client.insert("INSERT INTO e2e_json_max_dyn_types VALUES", block, None).await?;
        while let Some(res) = stream.next().await {
            res?;
        }

        // Verify row count
        #[derive(clickhouse_arrow_derive::Row, Debug)]
        struct Cnt {
            count: u64,
        }
        let mut cnt_stream = client
            .query::<Cnt>("SELECT count() AS count FROM e2e_json_max_dyn_types", None)
            .await?;
        let mut total = 0u64;
        while let Some(r) = cnt_stream.next().await {
            total += r?.count;
        }
        assert_eq!(total, 4, "expected 4 rows inserted");

        // Fetch rows as JSON strings and validate 'value' is present and well-formed
        #[derive(clickhouse_arrow_derive::Row, Debug)]
        struct RowOut {
            data: String,
        }
        let mut rows_out = client
            .query::<RowOut>(
                "SELECT toJSONString(data) AS data FROM e2e_json_max_dyn_types ORDER BY data",
                None,
            )
            .await?;

        let mut seen = Vec::new();
        while let Some(r) = rows_out.next().await {
            seen.push(r?.data);
        }
        assert_eq!(seen.len(), 4);

        // Basic semantic checks: each row should contain a 'value' key, reconstructed by server
        let mut has_str = false;
        let mut has_int = false;
        let mut has_float = false;
        let mut has_array = false;

        for js in &seen {
            let v: serde_json::Value = serde_json::from_str(js)?;
            assert!(v.get("value").is_some(), "value missing in {js}");
            match v.get("value").unwrap() {
                serde_json::Value::String(s) if s == "str" => has_str = true,
                serde_json::Value::Number(n) if n.as_i64() == Some(42) => has_int = true,
                serde_json::Value::Number(n) if n.as_f64().is_some() => has_float = true,
                serde_json::Value::Array(a) if !a.is_empty() => has_array = true,
                _ => {}
            }
        }

        assert!(has_str && has_int && has_float && has_array, "unexpected row set: {seen:?}");

        // Cleanup
        client.execute("DROP TABLE e2e_json_max_dyn_types", None).await?;
        Ok(())
    }
}
