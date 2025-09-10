// use bytes::BytesMut;

use super::DeserializerState;
use super::protocol_data::{EmptyBlock, ProtocolData};
use crate::Type;
use crate::client::connection::ClientMetadata;
// use crate::compression::compress_data_sync;
use crate::io::{ClickHouseRead, ClickHouseWrite};
use crate::native::block::Block;
// Already imported as `super::DeserializerState`
use crate::native::protocol::CompressionMethod;
use crate::prelude::*;

/// Marker for Native format.
///
/// Read native `ClickHouse` blocks into this library's `Block` struct and write `Block`s into the
/// provided writer.
#[derive(Debug, Clone, Copy)]
pub struct NativeFormat {}

impl ClientFormat for NativeFormat {
    type Data = Block;

    const FORMAT: &'static str = "Native";
}

impl super::sealed::ClientFormatImpl<Block> for NativeFormat {
    type Deser = ();
    type Schema = Vec<(String, Type)>;
    type Ser = ();

    async fn read<R: ClickHouseRead + 'static>(
        reader: &mut R,
        revision: u64,
        metadata: ClientMetadata,
        state: &mut DeserializerState,
    ) -> Result<Option<Block>> {
        Ok(if let CompressionMethod::None = metadata.compression {
            Block::read_async(reader, revision, None, state).await?.into_option()
        } else {
            // Stream-decompress all chunks for this packet and read block asynchronously - NOTE:
            // this means that effectively we no longer use the sync deserialization code.
            // This strat is simpler because with async block processing we properly handle
            // multiple compressed chunks per block. The next todo should be to add an async
            // compression writer and make the write path fully async as well. Then remove all
            // the duplicative sync serialization/deserialization code.
            let mut decompressor =
                crate::compression::DecompressionReader::new(metadata.compression, reader).await?;
            Block::read_async(&mut decompressor, revision, None, state).await?.into_option()
        })
    }

    async fn write<W: ClickHouseWrite>(
        writer: &mut W,
        data: Block,
        qid: Qid,
        header: Option<&[(String, Type)]>,
        revision: u64,
        metadata: ClientMetadata,
    ) -> Result<()> {
        // No-op: avoid noisy header logs in normal operation
        if let CompressionMethod::None = metadata.compression {
            data.write_async(writer, revision, header, Some(metadata))
                .instrument(trace_span!("serialize_block"))
                .await
                .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "(block:uncompressed)"))
        } else {
            // Stream-compress while writing the block to avoid buffering the whole block in memory
            use tokio::io::AsyncWriteExt as _;
            let mut sc = crate::compression::StreamingCompressor::new(
                writer,
                metadata.compression,
                1 << 20, // 1 MiB chunks (consider exposing via ClientOptions in the future)
            );
            let res = data
                .write_async(&mut sc, revision, header, Some(metadata))
                .instrument(trace_span!("serialize_block_streaming"))
                .await
                .inspect_err(|error| error!(?error, {ATT_QID} = %qid, "(block:streaming-compressed)"));
            // Ensure all frames are flushed; do NOT shutdown the underlying socket here.
            if let Err(e) = sc.flush().await { error!(?e, {ATT_QID} = %qid, "flush compressor"); }
            res
        }
    }
}
