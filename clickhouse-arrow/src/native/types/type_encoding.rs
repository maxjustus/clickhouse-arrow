//! Binary type encoding/decoding for ClickHouse native protocol
//!
//! This module implements encoding and decoding of type information as used in
//! shared data blobs within JSON/Object columns. Type bytes are defined in
//! ClickHouse's DataTypesBinaryEncoding.h

use std::io::{Read, Write};

use crate::io::{ClickHouseRead, ClickHouseWrite};
use crate::native::types::Type;
use crate::{Error, Result};

// Type encoding constants from DataTypesBinaryEncoding.h
const TYPE_NOTHING: u8 = 0x00;
const TYPE_UINT8: u8 = 0x01;
const TYPE_UINT16: u8 = 0x02;
const TYPE_UINT32: u8 = 0x03;
const TYPE_UINT64: u8 = 0x04;
const TYPE_UINT128: u8 = 0x05;
const TYPE_UINT256: u8 = 0x06;
const TYPE_INT8: u8 = 0x07;
const TYPE_INT16: u8 = 0x08;
const TYPE_INT32: u8 = 0x09;
const TYPE_INT64: u8 = 0x0A;
const TYPE_INT128: u8 = 0x0B;
const TYPE_INT256: u8 = 0x0C;
const TYPE_FLOAT32: u8 = 0x0D;
const TYPE_FLOAT64: u8 = 0x0E;
const TYPE_DATE: u8 = 0x0F;
const TYPE_DATE32: u8 = 0x10;
const TYPE_DATETIME: u8 = 0x11;
const TYPE_DATETIME64: u8 = 0x12;
const TYPE_STRING: u8 = 0x15;
const TYPE_FIXED_STRING: u8 = 0x16;
const TYPE_UUID: u8 = 0x1D;
const TYPE_ARRAY: u8 = 0x1E;
const TYPE_TUPLE: u8 = 0x1F;
const TYPE_NULLABLE: u8 = 0x23;
const TYPE_IPV4: u8 = 0x25;
const TYPE_IPV6: u8 = 0x26;
const TYPE_MAP: u8 = 0x27;
const TYPE_BOOL: u8 = 0x2D;

/// Decode a type from its binary encoding
pub fn decode_type<R: Read + ClickHouseRead>(reader: &mut R) -> Result<Type> {
    let type_byte = reader.read_u8()?;

    match type_byte {
        TYPE_NOTHING => Ok(Type::Nothing),
        TYPE_UINT8 => Ok(Type::UInt8),
        TYPE_UINT16 => Ok(Type::UInt16),
        TYPE_UINT32 => Ok(Type::UInt32),
        TYPE_UINT64 => Ok(Type::UInt64),
        TYPE_UINT128 => Ok(Type::UInt128),
        TYPE_UINT256 => Ok(Type::UInt256),
        TYPE_INT8 => Ok(Type::Int8),
        TYPE_INT16 => Ok(Type::Int16),
        TYPE_INT32 => Ok(Type::Int32),
        TYPE_INT64 => Ok(Type::Int64),
        TYPE_INT128 => Ok(Type::Int128),
        TYPE_INT256 => Ok(Type::Int256),
        TYPE_FLOAT32 => Ok(Type::Float32),
        TYPE_FLOAT64 => Ok(Type::Float64),
        TYPE_DATE => Ok(Type::Date),
        TYPE_DATE32 => Ok(Type::Date32),
        TYPE_DATETIME => {
            // DateTime with timezone - for shared data we use UTC
            Ok(Type::DateTime(chrono_tz::UTC))
        }
        TYPE_DATETIME64 => {
            // DateTime64 has precision and timezone
            // For shared data we use UTC
            let precision = reader.read_varuint()? as usize;
            Ok(Type::DateTime64(precision, chrono_tz::UTC))
        }
        TYPE_STRING => Ok(Type::String),
        TYPE_FIXED_STRING => {
            let size = reader.read_varuint()? as usize;
            Ok(Type::FixedSizedString(size))
        }
        TYPE_UUID => Ok(Type::Uuid),
        TYPE_ARRAY => {
            let element_type = decode_type(reader)?;
            Ok(Type::Array(Box::new(element_type)))
        }
        TYPE_TUPLE => {
            let num_elements = reader.read_varuint()? as usize;
            let mut elements = Vec::with_capacity(num_elements);
            for _ in 0..num_elements {
                elements.push(decode_type(reader)?);
            }
            Ok(Type::Tuple(elements))
        }
        TYPE_NULLABLE => {
            let inner_type = decode_type(reader)?;
            Ok(Type::Nullable(Box::new(inner_type)))
        }
        TYPE_IPV4 => Ok(Type::Ipv4),
        TYPE_IPV6 => Ok(Type::Ipv6),
        TYPE_MAP => {
            let key_type = decode_type(reader)?;
            let value_type = decode_type(reader)?;
            Ok(Type::Map(Box::new(key_type), Box::new(value_type)))
        }
        TYPE_BOOL => Ok(Type::Bool),
        _ => Err(Error::Decode(format!("Unknown type byte: 0x{:02x}", type_byte))),
    }
}

/// Encode a type to its binary encoding
pub fn encode_type<W: Write + ClickHouseWrite>(writer: &mut W, ty: &Type) -> Result<()> {
    match ty {
        Type::Nothing => writer.write_u8(TYPE_NOTHING)?,
        Type::UInt8 => writer.write_u8(TYPE_UINT8)?,
        Type::UInt16 => writer.write_u8(TYPE_UINT16)?,
        Type::UInt32 => writer.write_u8(TYPE_UINT32)?,
        Type::UInt64 => writer.write_u8(TYPE_UINT64)?,
        Type::UInt128 => writer.write_u8(TYPE_UINT128)?,
        Type::UInt256 => writer.write_u8(TYPE_UINT256)?,
        Type::Int8 => writer.write_u8(TYPE_INT8)?,
        Type::Int16 => writer.write_u8(TYPE_INT16)?,
        Type::Int32 => writer.write_u8(TYPE_INT32)?,
        Type::Int64 => writer.write_u8(TYPE_INT64)?,
        Type::Int128 => writer.write_u8(TYPE_INT128)?,
        Type::Int256 => writer.write_u8(TYPE_INT256)?,
        Type::Float32 => writer.write_u8(TYPE_FLOAT32)?,
        Type::Float64 => writer.write_u8(TYPE_FLOAT64)?,
        Type::Date => writer.write_u8(TYPE_DATE)?,
        Type::Date32 => writer.write_u8(TYPE_DATE32)?,
        Type::DateTime(_) => {
            writer.write_u8(TYPE_DATETIME)?;
            // DateTime with timezone is handled separately in full serialization
        }
        Type::DateTime64(precision, _) => {
            writer.write_u8(TYPE_DATETIME64)?;
            writer.write_varuint(*precision as u64)?;
        }
        Type::String => writer.write_u8(TYPE_STRING)?,
        Type::FixedSizedString(size) => {
            writer.write_u8(TYPE_FIXED_STRING)?;
            writer.write_varuint(*size as u64)?;
        }
        Type::Uuid => writer.write_u8(TYPE_UUID)?,
        Type::Array(element_type) => {
            writer.write_u8(TYPE_ARRAY)?;
            encode_type(writer, element_type)?;
        }
        Type::Tuple(elements) => {
            writer.write_u8(TYPE_TUPLE)?;
            writer.write_varuint(elements.len() as u64)?;
            for element in elements {
                encode_type(writer, element)?;
            }
        }
        Type::Nullable(inner_type) => {
            writer.write_u8(TYPE_NULLABLE)?;
            encode_type(writer, inner_type)?;
        }
        Type::Ipv4 => writer.write_u8(TYPE_IPV4)?,
        Type::Ipv6 => writer.write_u8(TYPE_IPV6)?,
        Type::Map(key_type, value_type) => {
            writer.write_u8(TYPE_MAP)?;
            encode_type(writer, key_type)?;
            encode_type(writer, value_type)?;
        }
        Type::Bool => writer.write_u8(TYPE_BOOL)?,
        _ => return Err(Error::Encode(format!("Unsupported type for binary encoding: {:?}", ty))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn test_encode_decode_primitives() {
        let types = vec![
            Type::Nothing,
            Type::Bool,
            Type::UInt8,
            Type::UInt32,
            Type::Int64,
            Type::Float32,
            Type::Float64,
            Type::String,
            Type::UUID,
            Type::Date,
        ];

        for ty in types {
            let mut buf = Vec::new();
            encode_type(&mut buf, &ty).unwrap();
            let mut cursor = Cursor::new(buf);
            let decoded = decode_type(&mut cursor).unwrap();
            assert_eq!(ty, decoded);
        }
    }

    #[test]
    fn test_encode_decode_array() {
        let ty = Type::Array(Box::new(Type::String));
        let mut buf = Vec::new();
        encode_type(&mut buf, &ty).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = decode_type(&mut cursor).unwrap();
        assert_eq!(ty, decoded);
    }

    #[test]
    fn test_encode_decode_nullable() {
        let ty = Type::Nullable(Box::new(Type::Int64));
        let mut buf = Vec::new();
        encode_type(&mut buf, &ty).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = decode_type(&mut cursor).unwrap();
        assert_eq!(ty, decoded);
    }

    #[test]
    fn test_encode_decode_map() {
        let ty = Type::Map(Box::new(Type::String), Box::new(Type::Int32));
        let mut buf = Vec::new();
        encode_type(&mut buf, &ty).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = decode_type(&mut cursor).unwrap();
        assert_eq!(ty, decoded);
    }

    #[test]
    fn test_encode_decode_nested() {
        let ty = Type::Array(Box::new(Type::Nullable(Box::new(Type::String))));
        let mut buf = Vec::new();
        encode_type(&mut buf, &ty).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = decode_type(&mut cursor).unwrap();
        assert_eq!(ty, decoded);
    }
}
