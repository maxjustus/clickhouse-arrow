use std::marker::PhantomData;

use tokio::io::{AsyncWriteExt, BufReader, BufWriter, stdin, stdout};

use crate::client::connection::ClientMetadata;
use crate::formats::DeserializerState;
use crate::formats::sealed::ClientFormatImpl;
use crate::io::{ClickHouseRead, ClickHouseWrite};
use crate::native::block::Block;
use crate::tracing::{error, trace};
use crate::{ArrowOptions, ClientFormat, CompressionMethod, NativeFormat, Qid, Result, Type};

/// A pragmatic, generic file-backed stream writer for `ClickHouse` native blocks.
///
/// - Works with any `T: ClientFormat` (Native or Arrow)
/// - Supports compressed and uncompressed streams via `CompressionMethod`
/// - Appends a terminator empty block on `finish()` so readers can stop cleanly
pub struct FileStreamWriter<T, W>
where
    T: ClientFormat,
    W: ClickHouseWrite,
{
    writer:   W,
    metadata: ClientMetadata,
    revision: u64,
    header:   Option<Vec<(String, Type)>>,
    _fmt:     PhantomData<T>,
}

impl<T, W> FileStreamWriter<T, W>
where
    T: ClientFormat,
    W: ClickHouseWrite,
{
    /// Create a new file-stream writer.
    ///
    /// - `writer`: any async writer (e.g., `tokio::fs::File` wrapped in a `BufWriter`)
    /// - `compression`: `None`, `LZ4`, or `ZSTD`
    /// - `arrow_options`: used when `T = ArrowFormat` (ignored for `NativeFormat`)
    /// - `header`: optional mapping to disambiguate column types on write
    pub fn new(
        writer: W,
        compression: CompressionMethod,
        arrow_options: ArrowOptions,
        header: Option<Vec<(String, Type)>>,
    ) -> Self {
        let metadata = ClientMetadata {
            client_id: 0, // not used for files
            compression,
            arrow_options,
            server_version: None,
        };
        // Use revision=0 to maximize file compatibility with `clickhouse local` Native output,
        // which omits BlockInfo in files.
        Self { writer, metadata, revision: 0, header, _fmt: PhantomData }
    }

    /// Write a single block/batch to the stream.
    pub async fn write(&mut self, data: T::Data) -> Result<()> {
        let qid = Qid::default();
        let header_ref = self.header.as_deref();
        trace!(format = T::FORMAT, "file_stream: write item");
        T::write(&mut self.writer, data, qid, header_ref, self.revision, self.metadata).await
    }

    /// Finish the stream by appending an empty terminator block and flushing.
    ///
    /// The terminator is an empty native block (`columns=0, rows=0`). It is safe for both
    /// `NativeFormat` and `ArrowFormat` readers because no column payload is present.
    pub async fn finish(&mut self) -> Result<()> {
        trace!(format = T::FORMAT, "file_stream: finish (write terminator)");
        // Use NativeFormat to write the empty terminator; it is format-agnostic when empty.
        let empty = Block { info: Default::default(), rows: 0, ..Default::default() };
        NativeFormat::write(
            &mut self.writer,
            empty,
            Qid::default(),
            None,
            self.revision,
            self.metadata,
        )
        .await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Access the inner writer (by reference).
    pub fn writer(&self) -> &W { &self.writer }

    /// Access the inner writer (by mutable reference).
    pub fn writer_mut(&mut self) -> &mut W { &mut self.writer }
}

/// A generic file-backed stream reader for `ClickHouse` native blocks.
///
/// Call `next()` repeatedly until it returns `Ok(None)` to consume the stream.
pub struct FileStreamReader<T, R>
where
    T: ClientFormat,
    R: ClickHouseRead,
{
    reader:   R,
    metadata: ClientMetadata,
    revision: u64,
    state:    DeserializerState<<T as ClientFormatImpl<T::Data>>::Deser>,
    _fmt:     PhantomData<T>,
}

impl<T, R> FileStreamReader<T, R>
where
    T: ClientFormat,
    R: ClickHouseRead + 'static,
{
    /// Create a new file-stream reader.
    ///
    /// - `reader`: any async reader (e.g., `tokio::fs::File` wrapped in a `BufReader`)
    /// - `compression`: `None`, `LZ4`, or `ZSTD` (must match how the stream was written)
    /// - `arrow_options`: used when `T = ArrowFormat` (ignored for `NativeFormat`)
    pub fn new(reader: R, compression: CompressionMethod, arrow_options: ArrowOptions) -> Self {
        let metadata = ClientMetadata {
            client_id: 0, // not used for files
            compression,
            arrow_options,
            server_version: None,
        };
        // Use revision=0 to match files produced by ClickHouse `Native` format (no BlockInfo).
        Self {
            reader,
            metadata,
            revision: 0,
            state: DeserializerState::default().with_arrow_options(arrow_options),
            _fmt: PhantomData,
        }
    }

    /// Read the next block/batch. Returns `Ok(None)` at stream end.
    pub async fn next(&mut self) -> Result<Option<T::Data>> {
        trace!(format = T::FORMAT, "file_stream: read next item");
        let res = T::read(&mut self.reader, self.revision, self.metadata, &mut self.state).await;
        match res {
            Ok(opt) => Ok(opt),
            Err(e) => {
                if let crate::Error::Io(ioe) = &e {
                    if ioe.kind() == std::io::ErrorKind::UnexpectedEof {
                        // Graceful EOF for files that omit a trailing empty block
                        return Ok(None);
                    }
                }
                error!(?e, "file_stream: read error");
                Err(e)
            }
        }
    }

    /// Convenience helper to read the entire stream into a `Vec`.
    pub async fn read_all(&mut self) -> Result<Vec<T::Data>> {
        let mut out = Vec::new();
        while let Some(item) = self.next().await? {
            out.push(item);
        }
        Ok(out)
    }

    /// Access the inner reader (by reference).
    pub fn reader(&self) -> &R { &self.reader }

    /// Access the inner reader (by mutable reference).
    pub fn reader_mut(&mut self) -> &mut R { &mut self.reader }
}

/// Convenience wrapper to stream native blocks to `stdout`.
pub struct NativeStdStreamWriter<W>
where
    W: ClickHouseWrite,
{
    inner: FileStreamWriter<NativeFormat, W>,
}

impl<W> NativeStdStreamWriter<W>
where
    W: ClickHouseWrite,
{
    /// Create a writer backed by an arbitrary `ClickHouseWrite` implementor.
    pub fn with_writer(
        writer: W,
        compression: CompressionMethod,
        header: Option<Vec<(String, Type)>>,
    ) -> Self {
        let inner = FileStreamWriter::new(writer, compression, ArrowOptions::default(), header);
        Self { inner }
    }

    /// Write a block to the underlying writer.
    pub async fn write(&mut self, block: Block) -> Result<()> { self.inner.write(block).await }

    /// Finish writing; appends a terminator block and flushes.
    pub async fn finish(&mut self) -> Result<()> { self.inner.finish().await }

    /// Access the underlying `FileStreamWriter`.
    pub fn inner(&self) -> &FileStreamWriter<NativeFormat, W> { &self.inner }

    /// Access the underlying `FileStreamWriter` mutably.
    pub fn inner_mut(&mut self) -> &mut FileStreamWriter<NativeFormat, W> { &mut self.inner }
}

impl NativeStdStreamWriter<BufWriter<tokio::io::Stdout>> {
    /// Create a writer that emits native blocks to `stdout`.
    pub fn new(compression: CompressionMethod, header: Option<Vec<(String, Type)>>) -> Self {
        let stdout = stdout();
        let writer = BufWriter::new(stdout);
        Self::with_writer(writer, compression, header)
    }
}

/// Convenience wrapper to stream native blocks from `stdin`.
pub struct NativeStdStreamReader<R>
where
    R: ClickHouseRead + 'static,
{
    inner: FileStreamReader<NativeFormat, R>,
}

impl<R> NativeStdStreamReader<R>
where
    R: ClickHouseRead + 'static,
{
    /// Create a reader backed by an arbitrary `ClickHouseRead` implementor.
    pub fn with_reader(reader: R, compression: CompressionMethod) -> Self {
        let inner = FileStreamReader::new(reader, compression, ArrowOptions::default());
        Self { inner }
    }

    /// Read the next native block.
    pub async fn next(&mut self) -> Result<Option<Block>> { self.inner.next().await }

    /// Read all remaining native blocks.
    pub async fn read_all(&mut self) -> Result<Vec<Block>> { self.inner.read_all().await }

    /// Access the underlying `FileStreamReader`.
    pub fn inner(&self) -> &FileStreamReader<NativeFormat, R> { &self.inner }

    /// Access the underlying `FileStreamReader` mutably.
    pub fn inner_mut(&mut self) -> &mut FileStreamReader<NativeFormat, R> { &mut self.inner }
}

impl NativeStdStreamReader<BufReader<tokio::io::Stdin>> {
    /// Create a reader that consumes native blocks from `stdin`.
    pub fn new(compression: CompressionMethod) -> Self {
        let stdin = stdin();
        let reader = BufReader::new(stdin);
        Self::with_reader(reader, compression)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use tokio::io::duplex;

    use super::*;
    use crate::{ArrowFormat, CompressionMethod};

    // Arrow round trip: compressed, multi-block
    #[tokio::test]
    async fn arrow_round_trip_compressed_multi_block() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let b1 = RecordBatch::try_new(Arc::clone(&schema), vec![
            Arc::new(Int32Array::from(vec![1, 2])),
            Arc::new(StringArray::from(vec!["a", "b"])),
        ])
        .unwrap();
        let b2 = RecordBatch::try_new(Arc::clone(&schema), vec![
            Arc::new(Int32Array::from(vec![3, 4, 5])),
            Arc::new(StringArray::from(vec!["c", "d", "e"])),
        ])
        .unwrap();

        // Write two batches with LZ4 compression
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = FileStreamWriter::<ArrowFormat, _>::new(
                &mut buf,
                CompressionMethod::LZ4,
                ArrowOptions::default().with_strings_as_strings(true),
                None,
            );
            writer.write(b1.clone()).await.unwrap();
            writer.write(b2.clone()).await.unwrap();
            writer.finish().await.unwrap();
        }

        // Read back
        let mut reader = FileStreamReader::<ArrowFormat, _>::new(
            Cursor::new(buf.into_inner()),
            CompressionMethod::LZ4,
            ArrowOptions::default().with_strings_as_strings(true),
        );
        let out = reader.read_all().await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].schema(), schema);
        assert_eq!(out[1].schema(), schema);
        assert_eq!(out[0].num_rows(), 2);
        assert_eq!(out[1].num_rows(), 3);
        // Spot-check values
        assert_eq!(out[0].column(0).as_any().downcast_ref::<Int32Array>().unwrap().values(), &[
            1, 2
        ]);
        assert_eq!(out[1].column(1).as_any().downcast_ref::<StringArray>().unwrap().value(2), "e");
    }

    // Native round trip: uncompressed, multi-block
    #[tokio::test]
    async fn native_round_trip_uncompressed_multi_block() {
        // Prepare two simple blocks: (id Int32, name String)
        let schema = vec![("id".to_string(), Type::Int32), ("name".to_string(), Type::String)];
        let blk1 = Block {
            info: Default::default(),
            rows: 2,
            column_types: schema.clone(),
            column_data: vec![
                // id column (2 rows)
                crate::native::values::Value::Int32(10),
                crate::native::values::Value::Int32(20),
                // name column (2 rows)
                crate::native::values::Value::String(b"alice".to_vec()),
                crate::native::values::Value::String(b"bob".to_vec()),
            ],
            ..Default::default()
        };
        let blk2 = Block {
            info: Default::default(),
            rows: 1,
            column_types: schema.clone(),
            column_data: vec![
                crate::native::values::Value::Int32(-1),
                // single row in id, then single row in name
                crate::native::values::Value::String(b"z".to_vec()),
            ],
            ..Default::default()
        };

        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = FileStreamWriter::<NativeFormat, _>::new(
                &mut buf,
                CompressionMethod::None,
                ArrowOptions::default(),
                Some(schema.clone()),
            );
            writer.write(blk1.clone()).await.unwrap();
            writer.write(blk2.clone()).await.unwrap();
            writer.finish().await.unwrap();
        }

        let mut reader = FileStreamReader::<NativeFormat, _>::new(
            Cursor::new(buf.into_inner()),
            CompressionMethod::None,
            ArrowOptions::default(),
        );

        let out = reader.read_all().await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rows, 2);
        assert_eq!(out[1].rows, 1);
        assert_eq!(out[0].column_types, schema);
        assert_eq!(out[1].column_types, schema);
    }

    #[tokio::test]
    async fn native_std_stream_round_trip() {
        let schema = vec![("id".to_string(), Type::Int32), ("name".to_string(), Type::String)];
        let block = Block {
            info: Default::default(),
            rows: 2,
            column_types: schema.clone(),
            column_data: vec![
                crate::native::values::Value::Int32(1),
                crate::native::values::Value::Int32(2),
                crate::native::values::Value::String(b"foo".to_vec()),
                crate::native::values::Value::String(b"bar".to_vec()),
            ],
            ..Default::default()
        };

        let (writer_stream, reader_stream) = duplex(64 * 1024);
        let mut writer = NativeStdStreamWriter::with_writer(
            BufWriter::new(writer_stream),
            CompressionMethod::None,
            Some(schema.clone()),
        );
        writer.write(block.clone()).await.unwrap();
        writer.finish().await.unwrap();

        let mut reader = NativeStdStreamReader::with_reader(
            BufReader::new(reader_stream),
            CompressionMethod::None,
        );
        let out = reader.read_all().await.unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rows, block.rows);
        assert_eq!(out[0].column_types, schema);
        assert_eq!(out[0].column_data, block.column_data);
    }
}
