// no extra imports

use super::{DeserializerState, Type};
use crate::Result;
// no need for SparseState/TypeSpecificState with plan-based sparse
use crate::io::ClickHouseRead;
use crate::native::values::Value;

const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;

async fn read_n_dense<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    n: usize,
) -> Result<Vec<Value>> {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use tokio::io::AsyncReadExt;
    use uuid::Uuid;

    use crate::{Date, Date32, DateTime, DynDateTime64, i256, u256};
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
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
                let value = pairs
                    .iter()
                    .find(|(_, i)| *i == idx)
                    .ok_or(crate::Error::DeserializeError(format!("Invalid enum8 index: {idx}")))?;
                Value::Enum8(value.0.clone(), idx)
            }
            Type::Enum16(pairs) => {
                let idx = reader.read_i16_le().await?;
                let value = pairs.iter().find(|(_, i)| *i == idx).ok_or(
                    crate::Error::DeserializeError(format!("Invalid enum16 index: {idx}")),
                )?;
                Value::Enum16(value.0.clone(), idx)
            }
            Type::String | Type::Binary => Value::String(reader.read_string().await?),
            Type::FixedSizedString(n) | Type::FixedSizedBinary(n) => {
                let mut buf = Vec::with_capacity(*n);
                unsafe { buf.set_len(*n) };
                let _ = reader.read_exact(&mut buf[..]).await?;
                let first_null = buf.iter().position(|x| *x == 0).unwrap_or(buf.len());
                buf.truncate(first_null);
                Value::String(buf)
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "Sparse dense-read not implemented for type: {type_:?}"
                )));
            }
        });
    }
    Ok(out)
}

//

pub(crate) async fn read_sparse_with_path<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>> {
    // Parse offsets
    let mut indices: Vec<usize> = Vec::new();
    let mut total_rows: usize;
    let mut tmp_offset: usize = 0;
    let mut skipped_values_rows: usize = 0;
    let mut first = true;

    let key = path.clone();
    let (mut trailing_defaults, mut has_value_after_defaults) =
        state.sparse_runtime.get(&key).copied().unwrap_or((0, false));

    total_rows = trailing_defaults;
    if has_value_after_defaults {
        if trailing_defaults >= tmp_offset {
            let start_of_group = 0;
            indices.push(start_of_group + trailing_defaults - tmp_offset);
            tmp_offset = 0;
            first = false;
        } else {
            skipped_values_rows += 1;
            tmp_offset = tmp_offset.saturating_sub(trailing_defaults + 1);
        }
        trailing_defaults = 0;
        total_rows += 1;
    }

    loop {
        let mut v = reader.read_var_uint().await?;
        let end = (v & END_OF_GRANULE_FLAG) != 0;
        if end {
            v &= !END_OF_GRANULE_FLAG;
        }
        let mut group_size = v as usize;

        let mut next_total_rows = total_rows + group_size;
        group_size += trailing_defaults;

        if next_total_rows >= rows {
            trailing_defaults = next_total_rows - rows;
            has_value_after_defaults = !end;
            break;
        }

        if end {
            has_value_after_defaults = false;
            trailing_defaults = group_size;
            break;
        } else {
            let start_of_group =
                if !first && !indices.is_empty() { indices[indices.len() - 1] + 1 } else { 0 };
            if group_size >= tmp_offset {
                indices.push(start_of_group + group_size - tmp_offset);
                tmp_offset = 0;
                first = false;
            } else {
                skipped_values_rows += 1;
                tmp_offset = tmp_offset.saturating_sub(group_size + 1);
            }
            trailing_defaults = 0;
            next_total_rows += 1;
        }
        total_rows = next_total_rows;
    }

    let _ = state.sparse_runtime.insert(key.clone(), (trailing_defaults, has_value_after_defaults));

    if skipped_values_rows > 0 {
        drop(read_n_dense(type_, reader, skipped_values_rows).await?);
    }
    let values = if !indices.is_empty() {
        read_n_dense(type_, reader, indices.len()).await?
    } else {
        Vec::new()
    };

    let _ = state.sparse_runtime.insert(key, (trailing_defaults, has_value_after_defaults));

    let mut out = vec![type_.default_value(); rows];
    for (i, v) in indices.into_iter().zip(values.into_iter()) {
        if i < rows {
            out[i] = v;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use tokio::io::{AsyncRead, ReadBuf};

    use super::*;

    // Minimal AsyncRead over Bytes
    struct BytesReader(bytes::Bytes);
    impl AsyncRead for BytesReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let to_copy = std::cmp::min(buf.remaining(), self.0.len());
            let chunk = self.0.split_to(to_copy);
            buf.put_slice(&chunk);
            std::task::Poll::Ready(Ok(()))
        }
    }

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
            if value == 0 {
                break;
            }
        }
        buf.extend_from_slice(&tmp[..pos]);
    }

    #[tokio::test]
    async fn test_sparse_string_generic_async() {
        // Build offsets for 10 rows: non-defaults at 1 and 5
        let mut bytes = BytesMut::new();
        put_var_uint(&mut bytes, 1); // defaults before first value
        put_var_uint(&mut bytes, 3); // gap to next value
        put_var_uint(&mut bytes, (1u64 << 62) | 4); // end-of-granule with trailing defaults

        // Elements: two strings "a", "bbb"
        // String layout: varUInt length + bytes
        put_var_uint(&mut bytes, 1);
        bytes.extend_from_slice(b"a");
        put_var_uint(&mut bytes, 3);
        bytes.extend_from_slice(b"bbb");

        let mut reader = BytesReader(bytes.freeze());
        let ty = Type::String;
        let mut state = DeserializerState::default();
        // No plan needed when calling read_sparse_async directly; runtime state starts empty

        let mut path = Vec::new();
        let out = read_sparse_with_path(&ty, &mut reader, 10, &mut state, &mut path).await.unwrap();
        assert_eq!(out.len(), 10);
        let get_str = |v: &Value| match v {
            Value::String(s) => String::from_utf8_lossy(s).to_string(),
            _ => panic!("expected String"),
        };
        assert_eq!(get_str(&out[0]), "");
        assert_eq!(get_str(&out[1]), "a");
        assert_eq!(get_str(&out[5]), "bbb");
        assert_eq!(get_str(&out[9]), "");
    }
}
