//! Binary value serialization for shared data in JSON/Object columns
//!
//! Values in shared data are encoded as [type_byte][value_bytes] where the type byte
//! identifies the type and is followed by the type-specific encoding.
//! Strings have NO null terminators in this format.

use std::io::Write;

use crate::io::ClickHouseWrite;
use crate::native::types::Type;
use crate::native::types::type_encoding::encode_type;
use crate::native::values::Value;
use crate::{Error, Result};

/// Serialize a value to binary format with type byte prefix
///
/// The output format is: [type_byte][value_data]
pub fn serialize_binary_value<W: Write + ClickHouseWrite>(
    writer: &mut W,
    value: &Value,
) -> Result<()> {
    let ty = infer_type(value)?;
    encode_type(writer, &ty)?;
    serialize_value_data(writer, value, &ty)
}

/// Infer the type from a value
fn infer_type(value: &Value) -> Result<Type> {
    match value {
        Value::Null => Ok(Type::Nothing),
        Value::Bool(_) => Ok(Type::Bool),
        Value::UInt8(_) => Ok(Type::UInt8),
        Value::UInt16(_) => Ok(Type::UInt16),
        Value::UInt32(_) => Ok(Type::UInt32),
        Value::UInt64(_) => Ok(Type::UInt64),
        Value::UInt128(_) => Ok(Type::UInt128),
        Value::UInt256(_) => Ok(Type::UInt256),
        Value::Int8(_) => Ok(Type::Int8),
        Value::Int16(_) => Ok(Type::Int16),
        Value::Int32(_) => Ok(Type::Int32),
        Value::Int64(_) => Ok(Type::Int64),
        Value::Int128(_) => Ok(Type::Int128),
        Value::Int256(_) => Ok(Type::Int256),
        Value::Float32(_) => Ok(Type::Float32),
        Value::Float64(_) => Ok(Type::Float64),
        Value::Date(_) => Ok(Type::Date),
        Value::Date32(_) => Ok(Type::Date32),
        Value::DateTime(_) => Ok(Type::DateTime(chrono_tz::UTC)),
        Value::DateTime64(precision, _) => Ok(Type::DateTime64(*precision, chrono_tz::UTC)),
        Value::String(_) => Ok(Type::String),
        Value::UUID(_) => Ok(Type::Uuid),
        Value::IPv4(_) => Ok(Type::Ipv4),
        Value::IPv6(_) => Ok(Type::Ipv6),
        Value::Array(elements) => {
            if elements.is_empty() {
                // Default to Array(Nothing) for empty arrays
                Ok(Type::Array(Box::new(Type::Nothing)))
            } else {
                let element_type = infer_type(&elements[0])?;
                Ok(Type::Array(Box::new(element_type)))
            }
        }
        Value::Tuple(elements) => {
            let element_types: Result<Vec<Type>> = elements.iter().map(infer_type).collect();
            Ok(Type::Tuple(element_types?))
        }
        Value::Map(keys, _values) => {
            if keys.is_empty() {
                Ok(Type::Map(Box::new(Type::Nothing), Box::new(Type::Nothing)))
            } else {
                let key_type = infer_type(&keys[0])?;
                let value_type = infer_type(&_values[0])?;
                Ok(Type::Map(Box::new(key_type), Box::new(value_type)))
            }
        }
        _ => Err(Error::Encode(format!("Cannot infer type for value: {:?}", value))),
    }
}

/// Serialize the value data (without type byte prefix)
fn serialize_value_data<W: Write + ClickHouseWrite>(
    writer: &mut W,
    value: &Value,
    ty: &Type,
) -> Result<()> {
    match (value, ty) {
        (Value::Null, Type::Nothing) => Ok(()),

        (Value::Bool(b), Type::Bool) => writer.write_u8(if *b { 1 } else { 0 })?,

        (Value::UInt8(v), Type::UInt8) => writer.write_u8(*v)?,
        (Value::UInt16(v), Type::UInt16) => writer.write_u16_le(*v)?,
        (Value::UInt32(v), Type::UInt32) => writer.write_u32_le(*v)?,
        (Value::UInt64(v), Type::UInt64) => writer.write_u64_le(*v)?,
        (Value::UInt128(v), Type::UInt128) => writer.write_u128_le(*v)?,
        (Value::UInt256(v), Type::UInt256) => writer.write_all(&v.to_le_bytes())?,

        (Value::Int8(v), Type::Int8) => writer.write_i8(*v)?,
        (Value::Int16(v), Type::Int16) => writer.write_i16_le(*v)?,
        (Value::Int32(v), Type::Int32) => writer.write_i32_le(*v)?,
        (Value::Int64(v), Type::Int64) => writer.write_i64_le(*v)?,
        (Value::Int128(v), Type::Int128) => writer.write_i128_le(*v)?,
        (Value::Int256(v), Type::Int256) => writer.write_all(&v.to_le_bytes())?,

        (Value::Float32(v), Type::Float32) => writer.write_f32_le(*v)?,
        (Value::Float64(v), Type::Float64) => writer.write_f64_le(*v)?,

        (Value::Date(d), Type::Date) => writer.write_u16_le(d.days_since_epoch())?,

        (Value::Date32(d), Type::Date32) => writer.write_i32_le(d.0)?,

        (Value::DateTime(dt), Type::DateTime(_)) => writer.write_u32_le(dt.timestamp())?,

        (Value::DateTime64(_precision, ticks), Type::DateTime64(_, _)) => {
            writer.write_i64_le(*ticks)?
        }

        (Value::String(s), Type::String) | (Value::String(s), Type::FixedSizedString(_)) => {
            writer.write_varuint(s.len() as u64)?;
            writer.write_all(s)?;
        }

        (Value::UUID(uuid), Type::Uuid) => {
            let bytes = uuid.as_bytes();
            // Write as two UInt64 LE (high, low)
            let high = u64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            let low = u64::from_be_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]);
            writer.write_u64_le(low)?;
            writer.write_u64_le(high)?;
        }

        (Value::IPv4(ip), Type::Ipv4) => writer.write_u32_le(u32::from(*ip))?,

        (Value::IPv6(ip), Type::Ipv6) => writer.write_all(&<[u8; 16]>::from(*ip))?,

        (Value::Array(elements), Type::Array(element_type)) => {
            writer.write_varuint(elements.len() as u64)?;
            for element in elements {
                serialize_value_data(writer, element, element_type)?;
            }
        }

        (Value::Tuple(elements), Type::Tuple(element_types)) => {
            if elements.len() != element_types.len() {
                return Err(Error::Encode("Tuple element count mismatch".to_string()));
            }
            for (element, element_type) in elements.iter().zip(element_types.iter()) {
                serialize_value_data(writer, element, element_type)?;
            }
        }

        (Value::Map(keys, values), Type::Map(key_type, value_type)) => {
            writer.write_varuint(keys.len() as u64)?;
            for key in keys {
                serialize_value_data(writer, key, key_type)?;
            }
            for value in values {
                serialize_value_data(writer, value, value_type)?;
            }
        }

        (Value::Null, Type::Nullable(_)) => writer.write_u8(1)?,

        (value, Type::Nullable(inner_type)) => {
            writer.write_u8(0)?; // not null
            serialize_value_data(writer, value, inner_type)?;
        }

        _ => {
            return Err(Error::Encode(format!(
                "Type mismatch: value {:?} does not match type {:?}",
                value, ty
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::values::Date;

    #[test]
    fn test_serialize_bool() {
        let mut buf = Vec::new();
        serialize_binary_value(&mut buf, &Value::Bool(true)).unwrap();
        assert_eq!(buf, vec![0x2d, 0x01]); // Type Bool + value true
    }

    #[test]
    fn test_serialize_int64() {
        let mut buf = Vec::new();
        serialize_binary_value(&mut buf, &Value::Int64(30)).unwrap();
        assert_eq!(buf, vec![0x0a, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_serialize_string() {
        let mut buf = Vec::new();
        serialize_binary_value(&mut buf, &Value::String(b"Alice".to_vec())).unwrap();
        assert_eq!(buf, vec![0x15, 0x05, 0x41, 0x6c, 0x69, 0x63, 0x65]);
    }

    #[test]
    fn test_serialize_float64() {
        let mut buf = Vec::new();
        serialize_binary_value(&mut buf, &Value::Float64(95.5)).unwrap();
        assert_eq!(buf, vec![0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x57, 0x40]);
    }

    #[test]
    fn test_serialize_array() {
        let mut buf = Vec::new();
        let array =
            Value::Array(vec![Value::String(b"admin".to_vec()), Value::String(b"user".to_vec())]);
        serialize_binary_value(&mut buf, &array).unwrap();

        // Should be: Array(String) + count=2 + "admin" + "user"
        let expected = vec![
            0x1e, // Array
            0x15, // String element type
            0x02, // count=2
            0x05, 0x61, 0x64, 0x6d, 0x69, 0x6e, // "admin"
            0x04, 0x75, 0x73, 0x65, 0x72, // "user"
        ];
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_serialize_nothing() {
        let mut buf = Vec::new();
        serialize_binary_value(&mut buf, &Value::Null).unwrap();
        assert_eq!(buf, vec![0x00]); // Just type byte
    }
}
