use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::AsyncReadExt;
use std::future::Future;
use uuid::Uuid;

use super::{Deserializer, DeserializerState, Type};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::{Date, Date32, DateTime, DynDateTime64, Result, i256, u256};

pub(crate) struct SizedDeserializer;

impl SizedDeserializer {
    // No sparse toggle parsing here; header plan is parsed in block.rs
}

impl Deserializer for SizedDeserializer {
    fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        _reader: &mut R,
        _state: &mut DeserializerState,
    ) -> impl Future<Output = Result<()>> { async move { Ok(()) } }
    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let mut path = Vec::new();
        read_with_path(type_, reader, rows, state, &mut path).await
    }

    // sync sized deserialization removed
}

pub(crate) async fn read_with_path<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>> {
    // If plan says SPARSE for current path, use generic sparse path
    let sparse_enabled = state
        .kind_plan
        .as_ref()
        .and_then(|p| p.get(path))
        .map(|&k| k != 0)
        .unwrap_or(false);

    if sparse_enabled {
        return super::sparse::read_sparse_with_path(type_, reader, rows, state, path).await;
    }

    // Dense sized read (copied from existing read)
    let mut out = Vec::with_capacity(rows);
    for _ in 0..rows {
        out.push(match type_ {
            Type::Int8 => Value::Int8(reader.read_i8().await?),
            Type::Int16 => Value::Int16(reader.read_i16_le().await?),
            Type::Int32 => Value::Int32(reader.read_i32_le().await?),
            Type::Int64 => Value::Int64(reader.read_i64_le().await?),
            Type::Int128 => Value::Int128(reader.read_i128_le().await?),
            Type::Int256 => {
                let mut buf = [0u8; 32];
                let _ = reader.read_exact(&mut buf[..]).await?;
                buf.reverse();
                Value::Int256(i256(buf))
            }
            Type::UInt8 => Value::UInt8(reader.read_u8().await?),
            Type::UInt16 => Value::UInt16(reader.read_u16_le().await?),
            Type::UInt32 => Value::UInt32(reader.read_u32_le().await?),
            Type::UInt64 => Value::UInt64(reader.read_u64_le().await?),
            Type::UInt128 => Value::UInt128(reader.read_u128_le().await?),
            Type::UInt256 => {
                let mut buf = [0u8; 32];
                let _ = reader.read_exact(&mut buf[..]).await?;
                buf.reverse();
                Value::UInt256(u256(buf))
            }
            Type::Float32 => Value::Float32(f32::from_bits(reader.read_u32_le().await?)),
            Type::Float64 => Value::Float64(f64::from_bits(reader.read_u64_le().await?)),
            Type::Decimal32(s) => Value::Decimal32(*s, reader.read_i32_le().await?),
            Type::Decimal64(s) => Value::Decimal64(*s, reader.read_i64_le().await?),
            Type::Decimal128(s) => Value::Decimal128(*s, reader.read_i128_le().await?),
            Type::Decimal256(s) => {
                let mut buf = [0u8; 32];
                let _ = reader.read_exact(&mut buf[..]).await?;
                buf.reverse();
                Value::Decimal256(*s, i256(buf))
            }
            Type::Uuid => Value::Uuid({
                let n1 = reader.read_u64_le().await?;
                let n2 = reader.read_u64_le().await?;
                Uuid::from_u128((u128::from(n1) << 64) | u128::from(n2))
            }),
            Type::Date => Value::Date(Date(reader.read_u16_le().await?)),
            Type::Date32 => Value::Date32(Date32(reader.read_i32_le().await?)),
            Type::DateTime(tz) => Value::DateTime(DateTime(*tz, reader.read_u32_le().await?)),
            Type::Ipv4 => Value::Ipv4(Ipv4Addr::from(reader.read_u32_le().await?).into()),
            Type::Ipv6 => {
                let mut octets = [0u8; 16];
                let _ = reader.read_exact(&mut octets[..]).await?;
                Value::Ipv6(Ipv6Addr::from(octets).into())
            }
            Type::DateTime64(precision, tz) => {
                let raw = reader.read_u64_le().await?;
                Value::DateTime64(DynDateTime64(*tz, raw, *precision))
            }
            Type::Enum8(pairs) => {
                let idx = reader.read_i8().await?;
                let value = pairs.iter().find(|(_, i)| *i == idx).ok_or(
                    crate::Error::DeserializeError(format!("Invalid enum8 index: {idx}")),
                )?;
                Value::Enum8(value.0.clone(), idx)
            }
            Type::Enum16(pairs) => {
                let idx = reader.read_i16_le().await?;
                let value = pairs.iter().find(|(_, i)| *i == idx).ok_or(
                    crate::Error::DeserializeError(format!("Invalid enum8 index: {idx}")),
                )?;
                Value::Enum16(value.0.clone(), idx)
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "SizedDeserializer unimplemented: {type_:?}"
                )));
            }
        });
    }
    Ok(out)
}

impl SizedDeserializer {
    pub(crate) fn read_prefix_sync(_type_: &Type, _reader: &mut impl ClickHouseBytesRead, _state: &mut DeserializerState) -> Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    // Prefix methods come via trait in other modules; not needed here.

    // Helper to write ClickHouse varUInt into a BytesMut
    fn put_var_uint(buf: &mut BytesMut, mut value: u64) {
        let mut tmp = [0u8; 9];
        let mut pos = 0;
        while pos < 9 {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value > 0 {
                byte |= 0x80;
            }
            tmp[pos] = byte;
            pos += 1;
            if value == 0 { break; }
        }
        buf.extend_from_slice(&tmp[..pos]);
    }

    // sync-only sparse test removed

    // toggle-based sparse prefix is not used for sized types; offsets terminate at end-of-granule flag
}
