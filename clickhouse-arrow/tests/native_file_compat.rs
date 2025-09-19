use std::env;
use std::io::Cursor;

use clickhouse_arrow::file_stream::FileStreamReader;
use clickhouse_arrow::{ArrowOptions, CompressionMethod, NativeFormat};

// Manual compatibility test. Requires a file produced by ClickHouse:
//   clickhouse local --query "select number from system.numbers limit 10 into outfile 'x.native' format Native"
// Then run:
//   CH_NATIVE_FILE=x.native cargo test -p clickhouse-arrow --test native_file_compat -- --ignored
#[tokio::test]
#[ignore]
async fn read_clickhouse_native_file() {
    let path = env::var("CH_NATIVE_FILE").expect("set CH_NATIVE_FILE to a Native file path");
    let compression = env::var("CH_NATIVE_COMPRESSION")
        .ok()
        .and_then(|s| s.parse::<CompressionMethod>().ok())
        .unwrap_or(CompressionMethod::None);

    let bytes = std::fs::read(&path).expect("read file");
    let cursor = Cursor::new(bytes);
    let mut reader = FileStreamReader::<NativeFormat, _>::new(
        cursor,
        compression,
        ArrowOptions::default(),
    );

    let mut total_rows = 0u64;
    let mut blocks = 0usize;
    while let Some(block) = reader.next().await.expect("read block") {
        blocks += 1;
        total_rows += block.rows;
    }

    // Quick sanity checks: some blocks, and total_rows > 0
    assert!(blocks >= 1);
    assert!(total_rows > 0);
}

