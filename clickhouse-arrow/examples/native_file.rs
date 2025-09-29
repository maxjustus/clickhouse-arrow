use std::env;

use clickhouse_arrow::file_stream::{FileStreamReader, FileStreamWriter};
use clickhouse_arrow::native::values::Value;
use clickhouse_arrow::{ArrowOptions, CompressionMethod, NativeFormat, Type};
use tokio::fs::File;
use tokio::io::{BufReader, BufWriter};

fn usage() {
    eprintln!(
        "Usage:\n  read:  cargo run --example native_file -- read  <path> [compression]\n  write: \
         cargo run --example native_file -- write <path> [count] [compression]\n\ncompression: \
         none | lz4 | zstd (default: none); count default: 10"
    );
}

fn parse_compression(s: Option<String>) -> CompressionMethod {
    s.and_then(|x| x.parse::<CompressionMethod>().ok()).unwrap_or(CompressionMethod::None)
}

#[tokio::main]
async fn main() {
    let mut args = env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => {
            usage();
            std::process::exit(2);
        }
    };

    match cmd.as_str() {
        "read" => {
            let path = match args.next() {
                Some(p) => p,
                None => {
                    usage();
                    std::process::exit(2);
                }
            };
            let compression = parse_compression(args.next());

            let file = match File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error opening {path}: {e}");
                    std::process::exit(1);
                }
            };
            let reader = BufReader::new(file);
            let mut reader = FileStreamReader::<NativeFormat, _>::new(
                reader,
                compression,
                ArrowOptions::default(),
            );

            let mut blocks = 0usize;
            let mut total_rows = 0u64;
            let mut global_row_idx: u64 = 0;
            while let Some(mut block) = reader.next().await.expect("read next block") {
                blocks += 1;
                println!("Block #{blocks}: rows={}", block.rows);
                if !block.column_types.is_empty() {
                    println!("Columns ({}):", block.column_types.len());
                    for (name, ty) in &block.column_types {
                        println!("- {}: {}", name, ty);
                    }
                }

                // Print rows
                for row in block.take_iter_rows() {
                    total_rows += 1;
                    global_row_idx += 1;
                    // Format: {name: value, ...}
                    let mut first = true;
                    let mut line = String::from("{");
                    for (name, _ty, value) in row {
                        if !first {
                            line.push_str(", ");
                        } else {
                            first = false;
                        }
                        line.push_str(name);
                        line.push_str(": ");
                        line.push_str(&format!("{:?}", value));
                    }
                    line.push('}');
                    println!("row {:>6} (block {:>6}): {}", global_row_idx, blocks, line);
                }
            }

            println!("--\nRead {blocks} block(s), total_rows={total_rows}");
        }
        "write" => {
            let path = match args.next() {
                Some(p) => p,
                None => {
                    usage();
                    std::process::exit(2);
                }
            };
            let count: u64 = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(10);
            let compression = parse_compression(args.next());

            let file = match File::create(&path).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error creating {path}: {e}");
                    std::process::exit(1);
                }
            };
            let writer = BufWriter::new(file);
            let mut writer = FileStreamWriter::<NativeFormat, _>::new(
                writer,
                compression,
                ArrowOptions::default(),
                Some(vec![("number".to_string(), Type::UInt64)]),
            );

            // Build a single-column (number UInt64) block with values 0..count-1
            let mut values = Vec::with_capacity(count as usize);
            for i in 0..count {
                values.push(Value::UInt64(i));
            }
            let block = clickhouse_arrow::native::block::Block {
                info: Default::default(),
                rows: count,
                column_types: vec![("number".to_string(), Type::UInt64)],
                column_data: values,
            };

            writer.write(block).await.expect("write block");
            writer.finish().await.expect("finish stream");
            println!("Wrote {count} rows to {path} with compression {compression}");
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    }
}
