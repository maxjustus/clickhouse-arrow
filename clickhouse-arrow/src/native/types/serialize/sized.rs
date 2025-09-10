use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::formats::TypeSpecificState;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Value};

pub(crate) struct SizedSerializer;

fn swap_endian_256(mut input: [u8; 32]) -> [u8; 32] {
    input.reverse();
    input
}

impl Serializer for SizedSerializer {
    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // If sparse/custom serialization requested for this column, write SparseOffsets + Elements
        if matches!(state.type_specific, TypeSpecificState::Sparse(_)) {
            let rows = values.len();
            // Collect non-default indices and values
            let mut indices = Vec::new();
            let mut elems = Vec::new();
            for (i, v) in values.into_iter().enumerate() {
                let is_default = match (type_.strip_null(), &v) {
                    (Type::Float32, Value::Float32(x)) => x.to_bits() == 0,
                    (Type::Float64, Value::Float64(x)) => x.to_bits() == 0,
                    (Type::Int8, Value::Int8(x)) => *x == 0,
                    (Type::Int16, Value::Int16(x)) => *x == 0,
                    (Type::Int32, Value::Int32(x)) => *x == 0,
                    (Type::Int64, Value::Int64(x)) => *x == 0,
                    (Type::UInt8, Value::UInt8(x)) => *x == 0,
                    (Type::UInt16, Value::UInt16(x)) => *x == 0,
                    (Type::UInt32, Value::UInt32(x)) => *x == 0,
                    (Type::UInt64, Value::UInt64(x)) => *x == 0,
                    (Type::Date, Value::Date(x)) => x.0 == 0,
                    (Type::Date32, Value::Date32(x)) => x.0 == 0,
                    (Type::DateTime(_), Value::DateTime(x)) => x.1 == 0,
                    (Type::DateTime64(_, _), Value::DateTime64(x)) => x.1 == 0,
                    (Type::Ipv4, Value::Ipv4(x)) => u32::from(x.0) == 0,
                    (Type::Ipv6, Value::Ipv6(x)) => x.octets() == [0u8; 16],
                    _ => false,
                };
                if !is_default {
                    indices.push(i);
                    elems.push(v);
                }
            }

            // Write offsets using group sizes and end-of-granule flag
            const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;
            let mut start = 0usize;
            for pos in &indices {
                let group = (*pos).saturating_sub(start) as u64;
                writer.write_var_uint(group).await?;
                start = *pos + 1;
            }
            let trailing = if start < rows { (rows - start) as u64 } else { 0u64 };
            writer.write_var_uint(trailing | END_OF_GRANULE_FLAG).await?;

            // Write elements in order
            for value in elems {
                match value.justify_null_ref(type_).as_ref() {
                    Value::Int8(x) | Value::Enum8(_, x) => writer.write_i8(*x).await?,
                    Value::Int16(x) | Value::Enum16(_, x) => writer.write_i16_le(*x).await?,
                    Value::Int32(x) | Value::Decimal32(_, x) => writer.write_i32_le(*x).await?,
                    Value::Int64(x) | Value::Decimal64(_, x) => writer.write_i64_le(*x).await?,
                    Value::UInt8(x) => writer.write_u8(*x).await?,
                    Value::UInt16(x) => writer.write_u16_le(*x).await?,
                    Value::UInt32(x) => writer.write_u32_le(*x).await?,
                    Value::UInt64(x) => writer.write_u64_le(*x).await?,
                    Value::Float32(x) => writer.write_u32_le(x.to_bits()).await?,
                    Value::Float64(x) => writer.write_u64_le(x.to_bits()).await?,
                    Value::Date(x) => writer.write_u16_le(x.0).await?,
                    Value::Date32(x) => writer.write_i32_le(x.0).await?,
                    Value::DateTime(x) => writer.write_u32_le(x.1).await?,
                    Value::DateTime64(x) => writer.write_u64_le(x.1).await?,
                    Value::Ipv4(x) => writer.write_u32_le(x.0.into()).await?,
                    Value::Ipv6(x) => writer.write_all(&x.octets()[..]).await?,
                    _ => return Err(Error::SerializeError(format!("Sparse sized write not implemented for {type_:?} value={value:?}"))),
                }
            }
            return Ok(());
        }
        for value in values {
            match value.justify_null_ref(type_).as_ref() {
                Value::Int8(x) | Value::Enum8(_, x) => writer.write_i8(*x).await?,
                Value::Int16(x) | Value::Enum16(_, x) => writer.write_i16_le(*x).await?,
                Value::Int32(x) | Value::Decimal32(_, x) => writer.write_i32_le(*x).await?,
                Value::Int64(x) | Value::Decimal64(_, x) => writer.write_i64_le(*x).await?,
                Value::Int128(x) | Value::Decimal128(_, x) => writer.write_i128_le(*x).await?,
                Value::Int256(x) | Value::Decimal256(_, x) => {
                    writer.write_all(&swap_endian_256(x.0)[..]).await?;
                }
                Value::UInt8(x) => writer.write_u8(*x).await?,
                Value::UInt16(x) => writer.write_u16_le(*x).await?,
                Value::UInt32(x) => writer.write_u32_le(*x).await?,
                Value::UInt64(x) => writer.write_u64_le(*x).await?,
                Value::UInt128(x) => writer.write_u128_le(*x).await?,
                Value::UInt256(x) => writer.write_all(&swap_endian_256(x.0)[..]).await?,
                Value::Float32(x) => writer.write_u32_le(x.to_bits()).await?,
                Value::Float64(x) => writer.write_u64_le(x.to_bits()).await?,
                Value::Uuid(x) => {
                    let n = x.as_u128();
                    let n1 = (n >> 64) as u64;
                    #[expect(clippy::cast_possible_truncation)]
                    let n2 = n as u64;
                    writer.write_u64_le(n1).await?;
                    writer.write_u64_le(n2).await?;
                }
                Value::Date(x) => writer.write_u16_le(x.0).await?,
                Value::Date32(x) => writer.write_i32_le(x.0).await?,
                Value::DateTime(x) => writer.write_u32_le(x.1).await?,
                Value::DateTime64(x) => writer.write_u64_le(x.1).await?,
                Value::Ipv4(x) => writer.write_u32_le(x.0.into()).await?,
                Value::Ipv6(x) => writer.write_all(&x.octets()[..]).await?,
                _ => {
                    return Err(Error::SerializeError(format!(
                        "SizedSerializer unimplemented: {type_:?} for value = {value:?}",
                    )));
                }
            }
        }
        Ok(())
    }

    fn write_sync(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut impl ClickHouseBytesWrite,
        state: &mut SerializerState,
    ) -> Result<()> {
        if matches!(state.type_specific, TypeSpecificState::Sparse(_)) {
            let rows = values.len();
            let mut indices = Vec::new();
            let mut elems = Vec::new();
            for (i, v) in values.into_iter().enumerate() {
                let is_default = match (type_.strip_null(), &v) {
                    (Type::Float32, Value::Float32(x)) => x.to_bits() == 0,
                    (Type::Float64, Value::Float64(x)) => x.to_bits() == 0,
                    (Type::Int8, Value::Int8(x)) => *x == 0,
                    (Type::Int16, Value::Int16(x)) => *x == 0,
                    (Type::Int32, Value::Int32(x)) => *x == 0,
                    (Type::Int64, Value::Int64(x)) => *x == 0,
                    (Type::UInt8, Value::UInt8(x)) => *x == 0,
                    (Type::UInt16, Value::UInt16(x)) => *x == 0,
                    (Type::UInt32, Value::UInt32(x)) => *x == 0,
                    (Type::UInt64, Value::UInt64(x)) => *x == 0,
                    (Type::Date, Value::Date(x)) => x.0 == 0,
                    (Type::Date32, Value::Date32(x)) => x.0 == 0,
                    (Type::DateTime(_), Value::DateTime(x)) => x.1 == 0,
                    (Type::DateTime64(_, _), Value::DateTime64(x)) => x.1 == 0,
                    (Type::Ipv4, Value::Ipv4(x)) => u32::from(x.0) == 0,
                    (Type::Ipv6, Value::Ipv6(x)) => x.octets() == [0u8; 16],
                    _ => false,
                };
                if !is_default {
                    indices.push(i);
                    elems.push(v);
                }
            }
            const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;
            let mut start = 0usize;
            for pos in &indices {
                let group = (*pos).saturating_sub(start) as u64;
                writer.put_var_uint(group)?;
                start = *pos + 1;
            }
            let trailing = if start < rows { (rows - start) as u64 } else { 0u64 };
            writer.put_var_uint(trailing | END_OF_GRANULE_FLAG)?;

            for value in elems {
                match value.justify_null_ref(type_).as_ref() {
                    Value::Int8(x) | Value::Enum8(_, x) => writer.put_i8(*x),
                    Value::Int16(x) | Value::Enum16(_, x) => writer.put_i16_le(*x),
                    Value::Int32(x) | Value::Decimal32(_, x) => writer.put_i32_le(*x),
                    Value::Int64(x) | Value::Decimal64(_, x) => writer.put_i64_le(*x),
                    Value::UInt8(x) => writer.put_u8(*x),
                    Value::UInt16(x) => writer.put_u16_le(*x),
                    Value::UInt32(x) => writer.put_u32_le(*x),
                    Value::UInt64(x) => writer.put_u64_le(*x),
                    Value::Float32(x) => writer.put_u32_le(x.to_bits()),
                    Value::Float64(x) => writer.put_u64_le(x.to_bits()),
                    Value::Date(x) => writer.put_u16_le(x.0),
                    Value::Date32(x) => writer.put_i32_le(x.0),
                    Value::DateTime(x) => writer.put_u32_le(x.1),
                    Value::DateTime64(x) => writer.put_u64_le(x.1),
                    Value::Ipv4(x) => writer.put_u32_le(x.0.into()),
                    Value::Ipv6(x) => writer.put_slice(&x.octets()[..]),
                    _ => return Err(Error::SerializeError(format!("Sparse sized write not implemented for {type_:?} value={value:?}"))),
                }
            }
            return Ok(());
        }
        for value in values {
            match value.justify_null_ref(type_).as_ref() {
                Value::Int8(x) | Value::Enum8(_, x) => writer.put_i8(*x),
                Value::Int16(x) | Value::Enum16(_, x) => writer.put_i16_le(*x),
                Value::Int64(x) | Value::Decimal64(_, x) => writer.put_i64_le(*x),
                Value::Int128(x) | Value::Decimal128(_, x) => writer.put_i128_le(*x),
                Value::Int256(x) | Value::Decimal256(_, x) => {
                    writer.put_slice(&swap_endian_256(x.0)[..]);
                }
                Value::UInt8(x) => writer.put_u8(*x),
                Value::UInt16(x) => writer.put_u16_le(*x),
                Value::UInt32(x) => writer.put_u32_le(*x),
                Value::UInt64(x) => writer.put_u64_le(*x),
                Value::UInt128(x) => writer.put_u128_le(*x),
                Value::UInt256(x) => writer.put_slice(&swap_endian_256(x.0)[..]),
                Value::Float32(x) => writer.put_u32_le(x.to_bits()),
                Value::Float64(x) => writer.put_u64_le(x.to_bits()),
                Value::Decimal32(_, x) | Value::Int32(x) => {
                    writer.put_i32_le(*x);
                }
                Value::Uuid(x) => {
                    let n = x.as_u128();
                    let n1 = (n >> 64) as u64;
                    #[expect(clippy::cast_possible_truncation)]
                    let n2 = n as u64;
                    writer.put_u64_le(n1);
                    writer.put_u64_le(n2);
                }
                Value::Date(x) => writer.put_u16_le(x.0),
                Value::Date32(x) => writer.put_i32_le(x.0),
                Value::DateTime(x) => writer.put_u32_le(x.1),
                Value::DateTime64(x) => writer.put_u64_le(x.1),
                Value::Ipv4(x) => writer.put_u32_le(x.0.into()),
                Value::Ipv6(x) => writer.put_slice(&x.octets()[..]),
                _ => {
                    return Err(Error::SerializeError(format!(
                        "SizedSerializer unimplemented: {type_:?} for value = {value:?}",
                    )));
                }
            }
        }
        Ok(())
    }
}
impl SizedSerializer {
    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // If we are going to use sparse/custom for this column, write a varUInt=1 toggle
        if matches!(state.type_specific, TypeSpecificState::Sparse(_)) {
            writer.write_var_uint(1).await?;
        }
        Ok(())
    }
}
