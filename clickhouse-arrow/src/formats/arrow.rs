use std::io::Cursor;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use tokio::io::AsyncWriteExt as _;

// use bytes::BytesMut;
use super::DeserializerState;
use super::protocol_data::ProtocolData;
use crate::Type;
use crate::arrow::ArrowDeserializerState;
use crate::compression::{StreamingCompressor, read_compressed_block};
use crate::connection::ClientMetadata;
use crate::io::{ClickHouseRead, ClickHouseWrite};
use crate::native::protocol::CompressionMethod;
use crate::native::sync::ReadAheadReader;
use crate::prelude::*;

/// Marker trait for Arrow format.
///
/// Read native `ClickHouse` blocks into arrow `RecordBatch`es and write arrow `RecordBatch`es into
/// native blocks.
#[derive(Debug, Clone, Copy)]
pub struct ArrowFormat {}

impl ClientFormat for ArrowFormat {
    type Data = RecordBatch;

    const FORMAT: &'static str = "Arrow";
}

impl super::sealed::ClientFormatImpl<RecordBatch> for ArrowFormat {
    type Deser = ArrowDeserializerState;
    type Schema = SchemaRef;
    type Ser = ();

    fn finish_deser(state: &mut DeserializerState<Self::Deser>) {
        state.deserializer().builders.clear();
        state.deserializer().buffer.clear();
    }

    async fn write<W: ClickHouseWrite>(
        writer: &mut W,
        batch: RecordBatch,
        qid: Qid,
        header: Option<&[(String, Type)]>,
        revision: u64,
        metadata: ClientMetadata,
    ) -> Result<()> {
        if let CompressionMethod::None = metadata.compression {
            batch
                .write_async(writer, revision, header, metadata.arrow_options)
                .instrument(trace_span!("serialize_block"))
                .await
                .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "serialize"))?;
        } else {
            // Stream-compress Arrow blocks during async serialization (avoid buffering entire
            // batch)
            let mut sc = StreamingCompressor::new(
                writer,
                metadata.compression,
                1 << 20, // 1 MiB chunks (consider exposing via ClientOptions)
            );
            batch
                .write_async(&mut sc, revision, header, metadata.arrow_options)
                .instrument(trace_span!("serialize_block_streaming"))
                .await
                .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "serialize"))?;
            // Flush frames but do not shutdown the underlying socket
            drop(sc.flush().await);
        }

        Ok(())
    }

    async fn read<R: ClickHouseRead + crate::native::sync::ReadAheadBuffer + 'static>(
        reader: &mut R,
        revision: u64,
        metadata: ClientMetadata,
        state: &mut DeserializerState<Self::Deser>,
    ) -> Result<Option<RecordBatch>> {
        let arrow_options = metadata.arrow_options;
        let batch = if let CompressionMethod::None = metadata.compression {
            RecordBatch::read_async(reader, revision, arrow_options, state).await.map(Some)
        } else if let Some(chunk) =
            read_compressed_block(reader, metadata.compression).await?
        {
            let mut buffered = ReadAheadReader::new(Cursor::new(chunk));
            RecordBatch::read_async(&mut buffered, revision, arrow_options, state).await.map(Some)
        } else {
            Ok(None)
        };
        batch
            .inspect_err(|error| error!(?error, "deserializing arrow record batch"))
    }
}
