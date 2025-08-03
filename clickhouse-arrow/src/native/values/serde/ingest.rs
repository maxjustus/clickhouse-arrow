use crate::native::types::Type;
use crate::native::values::Value;
use crate::{Error, Result};

pub fn cell_to_value(
    cell: Option<&serde_json::Value>,
    ty: &Type,
    strict: bool,
    on_missing_default: bool,
) -> Result<Value> {
    match (cell, ty) {
        // Missing cell
        (None, t) => {
            if t.is_nullable() {
                Ok(Value::Null)
            } else if on_missing_default {
                Ok(t.default_value())
            } else {
                Err(Error::SerializeError(format!("missing non-nullable column for type {t:?}")))
            }
        }
        // Present cell, handle JSON/Object specially
        (Some(v), Type::JSON { .. }) => {
            if v.is_null() {
                return Ok(Value::Null);
            }
            // Accept either JSON object/array/value or a string containing JSON
            if let serde_json::Value::String(s) = v {
                // Keep string inputs as strings (back-compat)
                let bytes = match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(parsed) => serde_json::to_vec(&parsed)
                        .map_err(|e| Error::SerializeError(e.to_string()))?,
                    Err(_) => {
                        serde_json::to_vec(v).map_err(|e| Error::SerializeError(e.to_string()))?
                    }
                };
                Ok(Value::String(bytes))
            } else {
                Ok(Value::Json(v.clone()))
            }
        }
        (Some(v), Type::Object) => {
            if v.is_null() {
                return Ok(Value::Null);
            }
            let bytes = if let serde_json::Value::String(s) = v {
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(parsed) => serde_json::to_vec(&parsed)
                        .map_err(|e| Error::SerializeError(e.to_string()))?,
                    Err(_) => {
                        serde_json::to_vec(v).map_err(|e| Error::SerializeError(e.to_string()))?
                    }
                }
            } else {
                serde_json::to_vec(v).map_err(|e| Error::SerializeError(e.to_string()))?
            };
            Ok(Value::Object(bytes))
        }
        // Nullable: recurse into inner type
        (Some(serde_json::Value::Null), t) => {
            if t.is_nullable() {
                Ok(Value::Null)
            } else {
                Ok(t.default_value())
            }
        }
        (Some(v), Type::Nullable(inner) | Type::LowCardinality(inner)) => {
            cell_to_value(Some(v), inner, strict, on_missing_default)
        }
        // Scalars
        (Some(v), Type::String | Type::FixedSizedString(_)) => match v {
            serde_json::Value::String(s) => Ok(Value::String(s.as_bytes().to_vec())),
            _ => Ok(Value::String(v.to_string().into_bytes())),
        },
        (Some(v), Type::UInt8) => to_u64(v, strict).and_then(u8_from_u64).map(Value::UInt8),
        (Some(v), Type::UInt16) => to_u64(v, strict).and_then(u16_from_u64).map(Value::UInt16),
        (Some(v), Type::UInt32) => to_u64(v, strict).and_then(u32_from_u64).map(Value::UInt32),
        (Some(v), Type::UInt64) => to_u64(v, strict).map(Value::UInt64),
        (Some(v), Type::Int8) => to_i64(v, strict).and_then(i8_from_i64).map(Value::Int8),
        (Some(v), Type::Int16) => to_i64(v, strict).and_then(i16_from_i64).map(Value::Int16),
        (Some(v), Type::Int32) => to_i64(v, strict).and_then(i32_from_i64).map(Value::Int32),
        (Some(v), Type::Int64) => to_i64(v, strict).map(Value::Int64),
        (Some(v), Type::Float32) => to_f64(v, strict).map(|x| Value::Float32(x as f32)),
        (Some(v), Type::Float64) => to_f64(v, strict).map(Value::Float64),
        // Fallback for unsupported complex types in MVP
        (_, t) => Err(Error::SerializeError(format!("unsupported type mapping for {t:?}"))),
    }
}

fn to_u64(v: &serde_json::Value, strict: bool) -> Result<u64> {
    if let Some(u) = v.as_u64() {
        return Ok(u);
    }
    if !strict {
        if let Some(i) = v.as_i64() {
            return Ok(i as u64);
        }
        if let Some(f) = v.as_f64() {
            return Ok(f as u64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<u64>().map_err(|e| Error::SerializeError(format!("parse u64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to u64".to_string()))
}

fn to_i64(v: &serde_json::Value, strict: bool) -> Result<i64> {
    if let Some(i) = v.as_i64() {
        return Ok(i);
    }
    if !strict {
        if let Some(u) = v.as_u64() {
            return Ok(u as i64);
        }
        if let Some(f) = v.as_f64() {
            return Ok(f as i64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<i64>().map_err(|e| Error::SerializeError(format!("parse i64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to i64".to_string()))
}

fn to_f64(v: &serde_json::Value, strict: bool) -> Result<f64> {
    if let Some(f) = v.as_f64() {
        return Ok(f);
    }
    if !strict {
        if let Some(i) = v.as_i64() {
            return Ok(i as f64);
        }
        if let Some(u) = v.as_u64() {
            return Ok(u as f64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<f64>().map_err(|e| Error::SerializeError(format!("parse f64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to f64".to_string()))
}

fn u8_from_u64(x: u64) -> Result<u8> {
    if x <= u8::MAX as u64 {
        Ok(x as u8)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt8")))
    }
}

fn u16_from_u64(x: u64) -> Result<u16> {
    if x <= u16::MAX as u64 {
        Ok(x as u16)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt16")))
    }
}

fn u32_from_u64(x: u64) -> Result<u32> {
    if x <= u32::MAX as u64 {
        Ok(x as u32)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt32")))
    }
}

fn i8_from_i64(x: i64) -> Result<i8> {
    if x >= i8::MIN as i64 && x <= i8::MAX as i64 {
        Ok(x as i8)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int8")))
    }
}

fn i16_from_i64(x: i64) -> Result<i16> {
    if x >= i16::MIN as i64 && x <= i16::MAX as i64 {
        Ok(x as i16)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int16")))
    }
}

fn i32_from_i64(x: i64) -> Result<i32> {
    if x >= i32::MIN as i64 && x <= i32::MAX as i64 {
        Ok(x as i32)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int32")))
    }
}
