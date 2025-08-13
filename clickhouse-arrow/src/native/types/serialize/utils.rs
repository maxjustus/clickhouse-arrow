use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::formats::SerializerState;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};

/// Write variable-sized discriminator based on `total_types` count
pub(crate) async fn write_discriminator_async<W: ClickHouseWrite>(
    writer: &mut W,
    discriminator: u64,
    total_types: usize,
) -> Result<()> {
    match total_types {
        0..=255 => {
            debug_assert!(discriminator <= 255);
            writer.write_u8(u8::try_from(discriminator).unwrap()).await?;
        }
        256..=65535 => {
            debug_assert!(discriminator <= 65535);
            writer.write_u16_le(u16::try_from(discriminator).unwrap()).await?;
        }
        65536..=4_294_967_295 => {
            writer.write_u32_le(u32::try_from(discriminator).unwrap()).await?;
        }
        _ => {
            writer.write_u64_le(discriminator).await?;
        }
    }
    Ok(())
}

/// Write variable-sized discriminator based on `total_types` count (sync version)
pub(crate) fn write_discriminator_sync<W: ClickHouseBytesWrite>(
    writer: &mut W,
    discriminator: u64,
    total_types: usize,
) {
    match total_types {
        0..=255 => {
            debug_assert!(discriminator <= 255);
            writer.put_u8(u8::try_from(discriminator).unwrap());
        }
        256..=65535 => {
            debug_assert!(discriminator <= 65535);
            writer.put_u16_le(u16::try_from(discriminator).unwrap());
        }
        65536..=4_294_967_295 => {
            writer.put_u32_le(u32::try_from(discriminator).unwrap());
        }
        _ => {
            writer.put_u64_le(discriminator);
        }
    }
}

/// Check if server supports complex types (Dynamic/JSON) that require ClickHouse >= 25.6
pub(crate) fn check_complex_type_server_version(
    state: &SerializerState,
    type_name: &str,
) -> Result<()> {
    if let Some((major, minor, _)) = state.server_version
        && (major < 25 || (major == 25 && minor < 6))
    {
        return Err(crate::Error::SerializeError(format!(
            "{type_name} type requires ClickHouse server version >= 25.6, got {major}.{minor}"
        )));
    }
    Ok(())
}