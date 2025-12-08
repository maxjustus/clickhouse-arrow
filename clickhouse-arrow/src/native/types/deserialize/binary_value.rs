//! Binary value deserialization for shared data in JSON/Object columns
//!
//! Values in shared data are encoded as [type_byte][value_bytes] where the type byte
//! identifies the type and is followed by the type-specific encoding.
//! Strings have NO null terminators in this format.

use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::native::types::Type;
use crate::native::types::type_encoding::decode_type;
use crate::native::values::{DateTime, DynDateTime64, Ipv4, Ipv6, Value, i256, u256};
use crate::{Date, Date32, Error, Result};

/// Synchronous reader extension trait for binary value deserialization
trait SyncReadExt: Read {
    fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    fn read_i8(&mut self) -> Result<i8> { Ok(self.read_u8()? as i8) }

    fn read_u16_le(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_i16_le(&mut self) -> Result<i16> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)?;
        Ok(i16::from_le_bytes(buf))
    }

    fn read_u32_le(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn read_i32_le(&mut self) -> Result<i32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(i32::from_le_bytes(buf))
    }

    fn read_u64_le(&mut self) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn read_i64_le(&mut self) -> Result<i64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(i64::from_le_bytes(buf))
    }

    fn read_u128_le(&mut self) -> Result<u128> {
        let mut buf = [0u8; 16];
        self.read_exact(&mut buf)?;
        Ok(u128::from_le_bytes(buf))
    }

    fn read_i128_le(&mut self) -> Result<i128> {
        let mut buf = [0u8; 16];
        self.read_exact(&mut buf)?;
        Ok(i128::from_le_bytes(buf))
    }

    fn read_f32_le(&mut self) -> Result<f32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(f32::from_le_bytes(buf))
    }

    fn read_f64_le(&mut self) -> Result<f64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(f64::from_le_bytes(buf))
    }

    fn read_varuint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0;

        loop {
            let byte = self.read_u8()?;
            result |= ((byte & 0x7F) as u64) << shift;

            if byte & 0x80 == 0 {
                break;
            }

            shift += 7;
            if shift >= 64 {
                return Err(Error::DeserializeError("VarUInt overflow".to_string()));
            }
        }

        Ok(result)
    }
}

impl<R: Read> SyncReadExt for R {}

/// Deserialize a binary-encoded value from a byte slice
///
/// The slice contains: [type_byte][value_data]
/// Returns the deserialized Value
pub fn deserialize_binary_value(data: &[u8]) -> Result<Value> {
    let mut cursor = std::io::Cursor::new(data);
    deserialize_binary_value_from_reader(&mut cursor)
}

/// Deserialize a binary-encoded value from a reader
pub fn deserialize_binary_value_from_reader<R: Read>(reader: &mut R) -> Result<Value> {
    let ty = decode_type(reader)?;
    deserialize_value_with_type(reader, &ty)
}

/// Deserialize a value given its type
fn deserialize_value_with_type<R: Read>(reader: &mut R, ty: &Type) -> Result<Value> {
    match ty {
        Type::Nothing => Ok(Value::Null),

        Type::Bool => {
            let byte = reader.read_u8()?;
            Ok(Value::Bool(byte != 0))
        }

        Type::UInt8 => Ok(Value::UInt8(reader.read_u8()?)),
        Type::UInt16 => Ok(Value::UInt16(reader.read_u16_le()?)),
        Type::UInt32 => Ok(Value::UInt32(reader.read_u32_le()?)),
        Type::UInt64 => Ok(Value::UInt64(reader.read_u64_le()?)),
        Type::UInt128 => Ok(Value::UInt128(reader.read_u128_le()?)),
        Type::UInt256 => {
            let mut bytes = [0u8; 32];
            reader.read_exact(&mut bytes)?;
            Ok(Value::UInt256(u256(bytes)))
        }

        Type::Int8 => Ok(Value::Int8(reader.read_i8()?)),
        Type::Int16 => Ok(Value::Int16(reader.read_i16_le()?)),
        Type::Int32 => Ok(Value::Int32(reader.read_i32_le()?)),
        Type::Int64 => Ok(Value::Int64(reader.read_i64_le()?)),
        Type::Int128 => Ok(Value::Int128(reader.read_i128_le()?)),
        Type::Int256 => {
            let mut bytes = [0u8; 32];
            reader.read_exact(&mut bytes)?;
            Ok(Value::Int256(i256(bytes)))
        }

        Type::Float32 => Ok(Value::Float32(reader.read_f32_le()?)),
        Type::Float64 => Ok(Value::Float64(reader.read_f64_le()?)),

        Type::Date => {
            let days = reader.read_u16_le()?;
            Ok(Value::Date(Date::from_days(days as i32)))
        }

        Type::Date32 => {
            let days = reader.read_i32_le()?;
            Ok(Value::Date32(Date32(days)))
        }

        Type::DateTime(tz) => {
            let timestamp = reader.read_u32_le()?;
            Ok(Value::DateTime(DateTime(*tz, timestamp)))
        }

        Type::DateTime64(precision, tz) => {
            let ticks = reader.read_i64_le()?;
            Ok(Value::DateTime64(DynDateTime64(*tz, ticks as u64, *precision)))
        }

        Type::String => {
            let len = reader.read_varuint()? as usize;
            let mut bytes = vec![0u8; len];
            reader.read_exact(&mut bytes)?;
            Ok(Value::String(bytes))
        }

        Type::FixedSizedString(size) => {
            let mut bytes = vec![0u8; *size];
            reader.read_exact(&mut bytes)?;
            Ok(Value::String(bytes))
        }

        Type::Uuid => {
            let low = reader.read_u64_le()?;
            let high = reader.read_u64_le()?;
            let uuid_bytes = [
                (high >> 56) as u8,
                (high >> 48) as u8,
                (high >> 40) as u8,
                (high >> 32) as u8,
                (high >> 24) as u8,
                (high >> 16) as u8,
                (high >> 8) as u8,
                high as u8,
                (low >> 56) as u8,
                (low >> 48) as u8,
                (low >> 40) as u8,
                (low >> 32) as u8,
                (low >> 24) as u8,
                (low >> 16) as u8,
                (low >> 8) as u8,
                low as u8,
            ];
            Ok(Value::Uuid(uuid::Uuid::from_bytes(uuid_bytes)))
        }

        Type::Ipv4 => {
            let value = reader.read_u32_le()?;
            Ok(Value::Ipv4(Ipv4(Ipv4Addr::from(value))))
        }

        Type::Ipv6 => {
            let mut bytes = [0u8; 16];
            reader.read_exact(&mut bytes)?;
            Ok(Value::Ipv6(Ipv6(Ipv6Addr::from(bytes))))
        }

        Type::Array(element_type) => {
            let count = reader.read_varuint()? as usize;
            let mut elements = Vec::with_capacity(count);
            for _ in 0..count {
                elements.push(deserialize_value_with_type(reader, element_type)?);
            }
            Ok(Value::Array(elements))
        }

        Type::Tuple(element_types) => {
            let mut elements = Vec::with_capacity(element_types.len());
            for element_type in element_types {
                elements.push(deserialize_value_with_type(reader, element_type)?);
            }
            Ok(Value::Tuple(elements))
        }

        Type::Nullable(inner_type) => {
            let is_null = reader.read_u8()?;
            if is_null != 0 {
                Ok(Value::Null)
            } else {
                deserialize_value_with_type(reader, inner_type)
            }
        }

        Type::Map(key_type, value_type) => {
            let count = reader.read_varuint()? as usize;
            let mut keys = Vec::with_capacity(count);
            let mut values = Vec::with_capacity(count);

            for _ in 0..count {
                keys.push(deserialize_value_with_type(reader, key_type)?);
            }
            for _ in 0..count {
                values.push(deserialize_value_with_type(reader, value_type)?);
            }

            Ok(Value::Map(keys, values))
        }

        _ => Err(Error::DeserializeError(format!(
            "Unsupported type for binary value deserialization: {:?}",
            ty
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_bool() {
        // Type byte 0x2D (Bool) + value 0x01 (true)
        let data = vec![0x2d, 0x01];
        let value = deserialize_binary_value(&data).unwrap();
        assert_eq!(value, Value::Bool(true));
    }

    #[test]
    fn test_deserialize_int64() {
        // Type byte 0x0A (Int64) + value 30 (0x1E) as 8-byte LE
        let data = vec![0x0a, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let value = deserialize_binary_value(&data).unwrap();
        assert_eq!(value, Value::Int64(30));
    }

    #[test]
    fn test_deserialize_string() {
        // Type byte 0x15 (String) + VarUInt(5) + "Alice"
        let data = vec![0x15, 0x05, 0x41, 0x6c, 0x69, 0x63, 0x65];
        let value = deserialize_binary_value(&data).unwrap();
        assert_eq!(value, Value::String(b"Alice".to_vec()));
    }

    #[test]
    fn test_deserialize_float64() {
        // Type byte 0x0E (Float64) + 95.5 as Float64 LE
        let data = vec![0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x57, 0x40];
        let value = deserialize_binary_value(&data).unwrap();
        if let Value::Float64(f) = value {
            assert!((f - 95.5).abs() < 0.001);
        } else {
            panic!("Expected Float64");
        }
    }

    #[test]
    fn test_deserialize_array_nullable_string() {
        // Type byte 0x1E (Array) + element type 0x23 (Nullable) + 0x15 (String)
        // + VarUInt count=2 + null_byte=0 + VarUInt(5) + "admin" + null_byte=0 + VarUInt(4) +
        //   "user"
        let data = vec![
            0x1e, // Array
            0x23, // Nullable
            0x15, // String
            0x02, // count=2
            0x00, // not null
            0x05, 0x61, 0x64, 0x6d, 0x69, 0x6e, // "admin"
            0x00, // not null
            0x04, 0x75, 0x73, 0x65, 0x72, // "user"
        ];
        let value = deserialize_binary_value(&data).unwrap();
        if let Value::Array(elements) = value {
            assert_eq!(elements.len(), 2);
            assert_eq!(elements[0], Value::String(b"admin".to_vec()));
            assert_eq!(elements[1], Value::String(b"user".to_vec()));
        } else {
            panic!("Expected Array");
        }
    }

    #[test]
    fn test_deserialize_nothing() {
        // Type byte 0x00 (Nothing)
        let data = vec![0x00];
        let value = deserialize_binary_value(&data).unwrap();
        assert_eq!(value, Value::Null);
    }
}
