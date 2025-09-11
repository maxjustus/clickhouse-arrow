use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::AsyncReadExt;
use std::future::Future;
use uuid::Uuid;

use super::{Deserializer, DeserializerState, Type};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::formats::{TypeSpecificState, SparseState};
use crate::{Date, Date32, DateTime, DynDateTime64, Result, i256, u256};

pub(crate) struct SizedDeserializer;

impl SizedDeserializer {
    // No extra flags here beyond the column-level toggle

    async fn read_sparse_values<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;
        let mut indices: Vec<usize> = Vec::new();
        let mut total_rows: usize;
        let mut tmp_offset: usize = 0; // rows_offset is 0 at block start
        let mut skipped_values_rows: usize = 0;
        let mut first = true;

        // Access sparse state across reads
        let (mut trailing_defaults, mut has_value_after_defaults) = match &state.type_specific {
            TypeSpecificState::Sparse(SparseState { num_trailing_defaults, has_value_after_defaults, .. }) => {
                (*num_trailing_defaults, *has_value_after_defaults)
            }
            _ => (0, false),
        };

        tracing::debug!(
            ty = ?type_, rows,
            prev_trailing_defaults = trailing_defaults,
            prev_has_value_after_defaults = has_value_after_defaults,
            "sparse(sized, async): start"
        );

        total_rows = trailing_defaults;

        // Handle pending value-after-defaults from prior early stop
        if has_value_after_defaults {
            if trailing_defaults >= tmp_offset {
                let start_of_group = 0; // start is 0 for fresh block
                indices.push(start_of_group + trailing_defaults - tmp_offset);
                tmp_offset = 0;
                first = false;
                tracing::trace!(action = "pending_value_push", index = indices.last().copied().unwrap_or(0));
            } else {
                skipped_values_rows += 1;
                tmp_offset = tmp_offset.saturating_sub(trailing_defaults + 1);
                tracing::trace!(action = "pending_value_skip", skipped_values_rows);
            }
            has_value_after_defaults = false;
            trailing_defaults = 0;
            total_rows += 1;
        }

        // Parse offsets with early stop at limit
        loop {
            let mut v = reader.read_var_uint().await?;
            let end = (v & END_OF_GRANULE_FLAG) != 0;
            if end { v &= !END_OF_GRANULE_FLAG; }
            let mut group_size = v as usize;

            let mut next_total_rows = total_rows + group_size;
            group_size += trailing_defaults;

            if next_total_rows >= rows {
                // carry state to next call
                trailing_defaults = next_total_rows - rows;
                has_value_after_defaults = !end;
                tracing::trace!(action = "early_stop", trailing_defaults, has_value_after_defaults);
                break;
            }

            if end {
                // End of current offsets substream for this read
                has_value_after_defaults = false;
                trailing_defaults = group_size;
                // Stop reading offsets; proceed to values
                tracing::trace!(action = "end_of_granule", trailing_defaults);
                break;
            } else {
                let start_of_group = if !first && !indices.is_empty() { indices[indices.len()-1] + 1 } else { 0 };
                if group_size >= tmp_offset {
                    indices.push(start_of_group + group_size - tmp_offset);
                    tmp_offset = 0;
                    first = false;
                    tracing::trace!(action = "push_index", index = indices.last().copied().unwrap_or(0));
                } else {
                    skipped_values_rows += 1;
                    tmp_offset = tmp_offset.saturating_sub(group_size + 1);
                    tracing::trace!(action = "skip_value", skipped_values_rows);
                }
                trailing_defaults = 0;
                has_value_after_defaults = false;
                next_total_rows += 1;
            }
            total_rows = next_total_rows;
        }

        // Persist state
        if let TypeSpecificState::Sparse(s) = &mut state.type_specific {
            s.num_trailing_defaults = trailing_defaults;
            s.has_value_after_defaults = has_value_after_defaults;
        }

        // Discard skipped values, then read exactly indices.len() values
        for _ in 0..skipped_values_rows {
            match type_ {
                Type::Int8 => { let _ = reader.read_i8().await?; }
                Type::Int16 => { let _ = reader.read_i16_le().await?; }
                Type::Int32 => { let _ = reader.read_i32_le().await?; }
                Type::Int64 => { let _ = reader.read_i64_le().await?; }
                Type::Int128 => { let _ = reader.read_i128_le().await?; }
                Type::Int256 => { let mut buf = [0u8; 32]; let _ = reader.read_exact(&mut buf[..]).await?; }
                Type::UInt8 => { let _ = reader.read_u8().await?; }
                Type::UInt16 => { let _ = reader.read_u16_le().await?; }
                Type::UInt32 => { let _ = reader.read_u32_le().await?; }
                Type::UInt64 => { let _ = reader.read_u64_le().await?; }
                Type::UInt128 => { let _ = reader.read_u128_le().await?; }
                Type::UInt256 => { let mut buf = [0u8; 32]; let _ = reader.read_exact(&mut buf[..]).await?; }
                Type::Float32 => { let _ = reader.read_u32_le().await?; }
                Type::Float64 => { let _ = reader.read_u64_le().await?; }
                Type::Decimal32(_) => { let _ = reader.read_i32_le().await?; }
                Type::Decimal64(_) => { let _ = reader.read_i64_le().await?; }
                Type::Decimal128(_) => { let _ = reader.read_i128_le().await?; }
                Type::Decimal256(_) => { let mut buf = [0u8; 32]; let _ = reader.read_exact(&mut buf[..]).await?; }
                Type::Date => { let _ = reader.read_u16_le().await?; }
                Type::Date32 => { let _ = reader.read_i32_le().await?; }
                Type::DateTime(_) => { let _ = reader.read_u32_le().await?; }
                Type::DateTime64(_, _) => { let _ = reader.read_u64_le().await?; }
                Type::Ipv4 => { let _ = reader.read_u32_le().await?; }
                Type::Ipv6 => { let mut buf = [0u8; 16]; let _ = reader.read_exact(&mut buf[..]).await?; }
                Type::Uuid => { let _ = reader.read_u64_le().await?; let _ = reader.read_u64_le().await?; }
                _ => return Err(crate::Error::DeserializeError(format!("Sparse skip not implemented for type: {type_:?}"))),
            }
        }

        // Now read the actual values for current indices
        let mut values = Vec::with_capacity(indices.len());
        for _ in 0..indices.len() {
            values.push(match type_ {
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
                Type::Uuid => {
                    let n1 = reader.read_u64_le().await?;
                    let n2 = reader.read_u64_le().await?;
                    Value::Uuid(Uuid::from_u128((u128::from(n1) << 64) | u128::from(n2)))
                }
                _ => return Err(crate::Error::DeserializeError(format!("Sparse deserialization not implemented for type: {type_:?}"))),
            });
        }

        let mut out = vec![type_.default_value(); rows];
        for (i, v) in indices.into_iter().zip(values.into_iter()) {
            if i < rows { out[i] = v; }
        }
        tracing::debug!(
            ty = ?type_, rows, indices_len = out.len(), skipped_values_rows,
            next_trailing_defaults = trailing_defaults, next_has_value_after_defaults = has_value_after_defaults,
            "sparse(sized, async): end"
        );
        Ok(out)
    }

}

impl Deserializer for SizedDeserializer {
    fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> impl Future<Output = Result<()>> {
        async move {
            if let TypeSpecificState::Sparse(SparseState { has_custom: true, use_custom, .. }) =
                &mut state.type_specific
            {
                if use_custom.is_none() {
                    let toggle = reader.read_u8().await?;
                    let use_flag = toggle != 0;
                    *use_custom = Some(use_flag);
                    tracing::debug!(toggle, use_custom = use_flag, ty = ?_type_, "sized sparse prefix toggle (async)");
                } else {
                    tracing::trace!(ty = ?_type_, "sized sparse toggle provided at column-level; skipping read");
                }
            }
            Ok(())
        }
    }
    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // If sparse/custom is enabled for this column, use sparse path
        let sparse_enabled = matches!(
            state.type_specific,
            TypeSpecificState::Sparse(SparseState { has_custom: true, use_custom: Some(true), .. })
        );

        if sparse_enabled {
            return Self::read_sparse_values(type_, reader, rows, state).await;
        }

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

    // sync sized deserialization removed
}

impl SizedDeserializer {
    pub(crate) fn read_prefix_sync(
        _type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        state: &mut DeserializerState,
    ) -> Result<()> {
        if let TypeSpecificState::Sparse(SparseState { has_custom: true, use_custom, .. }) =
            &mut state.type_specific
        {
            if use_custom.is_none() {
                let toggle = reader.try_get_var_uint()?;
                let use_flag = toggle != 0;
                *use_custom = Some(use_flag);
                tracing::debug!(toggle, use_custom = use_flag, ty = ?_type_, "sized sparse prefix toggle (sync)");
            }
        }
        Ok(())
    }
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
