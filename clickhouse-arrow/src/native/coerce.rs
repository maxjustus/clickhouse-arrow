use crate::native::types::Type;
use crate::native::values::{Date, Date32, DateTime, DynDateTime64, Value};
use crate::{Error, Result};

#[inline]
fn is_effectively_nullable(t: &Type) -> bool {
    match t {
        Type::Nullable(_) => true,
        Type::LowCardinality(inner) => inner.is_nullable(),
        _ => false,
    }
}

/// Determine discriminator and original type index for a value in a Variant type list.
///
/// The discriminator ordering follows the ClickHouse rule: inner types are ordered
/// alphabetically by their canonical string representation. The returned tuple is
/// (discriminator, original_index_in_variant_types).
///
/// Special case: Null maps to discriminator 0xFF.
pub(crate) fn discriminate(value: &Value, variant_types: &[Type]) -> Result<(u8, usize)> {
    // Null uses special discriminator and placeholder index 0
    if matches!(value, Value::Null) {
        return Ok((0xFF, 0));
    }

    // Build mapping from sorted type names to original indices
    let mut type_map: Vec<(String, usize)> =
        variant_types.iter().enumerate().map(|(i, t)| (t.to_string(), i)).collect();
    type_map.sort_by(|a, b| a.0.cmp(&b.0));

    // First try exact guessed-type match
    let guess = value.guess_type().to_string();
    if let Some((sorted_idx, (_, original_idx))) =
        type_map.iter().enumerate().find(|(_, (s, _))| s == &guess)
    {
        let disc = u8::try_from(sorted_idx).map_err(|_| {
            Error::SerializeError(format!("Too many variant types ({}), max 255", sorted_idx))
        })?;
        return Ok((disc, *original_idx));
    }

    // Then try convertible match (JSON behavior)
    if let Some((sorted_idx, (_, original_idx))) = type_map
        .iter()
        .enumerate()
        .find(|(_, (_name, idx))| can_convert_to_type(value, &variant_types[*idx]))
    {
        let disc = u8::try_from(sorted_idx).map_err(|_| {
            Error::SerializeError(format!("Too many variant types ({}), max 255", sorted_idx))
        })?;
        return Ok((disc, *original_idx));
    }

    Err(Error::SerializeError(format!("Cannot find matching variant type for value: {value:?}")))
}

/// Check if a value can be converted to a specific type (used for validation/fast paths).
pub(crate) fn can_convert_to_type(value: &Value, target_type: &Type) -> bool {
    match (value, target_type) {
        // Null can go to any Nullable type
        (Value::Null, Type::Nullable(_)) => true,
        (Value::Null, _) => false,

        // Exact primitive matches
        (Value::String(_), Type::String | Type::FixedSizedString(_)) => true,
        (Value::Int64(_), Type::Int64)
        | (Value::Float64(_), Type::Float64)
        | (Value::UInt64(_), Type::UInt64)
        | (Value::Int32(_), Type::Int32)
        | (Value::UInt32(_), Type::UInt32)
        | (Value::Float32(_), Type::Float32)
        | (Value::Int16(_), Type::Int16)
        | (Value::UInt16(_), Type::UInt16)
        | (Value::Int8(_), Type::Int8)
        | (Value::UInt8(_), Type::UInt8) => true,

        // Numeric conversions
        (
            Value::Int64(_) | Value::UInt64(_) | Value::Float64(_) | Value::Float32(_),
            Type::Int8
            | Type::Int16
            | Type::Int32
            | Type::Int64
            | Type::UInt8
            | Type::UInt16
            | Type::UInt32
            | Type::UInt64
            | Type::Float32
            | Type::Float64,
        ) => true,

        // Date/time from integer timestamps
        (
            Value::Int64(_) | Value::UInt64(_),
            Type::Date | Type::Date32 | Type::DateTime(_) | Type::DateTime64(_, _),
        ) => true,

        // Decimal from numeric
        (
            Value::Int64(_) | Value::UInt64(_) | Value::Float64(_),
            Type::Decimal32(_) | Type::Decimal64(_) | Type::Decimal128(_),
        ) => true,

        // Arrays and collections
        (Value::Array(_), Type::Array(_)) => true,
        (Value::Tuple(_), Type::Tuple(_) | Type::TupleNamed(_)) => true,
        // Map types and Array-of-pairs to Map
        (Value::Map(_, _), Type::Map(_, _)) | (Value::Array(_), Type::Map(_, _)) => true,

        // Recursive
        (v, Type::Nullable(inner)) => can_convert_to_type(v, inner),
        (v, Type::LowCardinality(inner)) => can_convert_to_type(v, inner),

        _ => false,
    }
}

/// Convert a value to a specific type, mirroring JSON typed-path coercion.
pub(crate) fn convert_to_type(value: Value, expected_type: &Type) -> Result<Value> {
    // Handle Nulls according to target type semantics
    if matches!(value, Value::Null) {
        return Ok(match expected_type {
            // Variant uses special null discriminator
            Type::Variant(_) => Value::Variant(0xFF, Box::new(Value::Null)),
            // Nullable and LC(Nullable) carry Null through
            _ if is_effectively_nullable(expected_type) => Value::Null,
            // Non-nullable types get their default value
            _ => expected_type.default_value(),
        });
    }

    match (value, expected_type) {
        // Exact matches
        (v @ Value::Int8(_), Type::Int8)
        | (v @ Value::Int16(_), Type::Int16)
        | (v @ Value::Int32(_), Type::Int32)
        | (v @ Value::Int64(_), Type::Int64)
        | (v @ Value::UInt8(_), Type::UInt8)
        | (v @ Value::UInt16(_), Type::UInt16)
        | (v @ Value::UInt32(_), Type::UInt32)
        | (v @ Value::UInt64(_), Type::UInt64)
        | (v @ Value::Float32(_), Type::Float32)
        | (v @ Value::Float64(_), Type::Float64)
        | (v @ Value::String(_), Type::String | Type::FixedSizedString(_)) => Ok(v),

        // Int64 → smaller ints/uints
        (Value::Int64(i), Type::Int8) => Ok(Value::Int8(i as i8)),
        (Value::Int64(i), Type::Int16) => Ok(Value::Int16(i as i16)),
        (Value::Int64(i), Type::Int32) => Ok(Value::Int32(i as i32)),
        (Value::Int64(i), Type::UInt8) => Ok(Value::UInt8(i as u8)),
        (Value::Int64(i), Type::UInt16) => Ok(Value::UInt16(i as u16)),
        (Value::Int64(i), Type::UInt32) => Ok(Value::UInt32(i as u32)),
        (Value::Int64(i), Type::UInt64) => Ok(Value::UInt64(i as u64)),

        // UInt64 → ints
        (Value::UInt64(u), Type::Int8) => Ok(Value::Int8(u as i8)),
        (Value::UInt64(u), Type::Int16) => Ok(Value::Int16(u as i16)),
        (Value::UInt64(u), Type::Int32) => Ok(Value::Int32(u as i32)),
        (Value::UInt64(u), Type::Int64) => Ok(Value::Int64(u as i64)),
        (Value::UInt64(u), Type::UInt8) => Ok(Value::UInt8(u as u8)),
        (Value::UInt64(u), Type::UInt16) => Ok(Value::UInt16(u as u16)),
        (Value::UInt64(u), Type::UInt32) => Ok(Value::UInt32(u as u32)),

        // Float → integer (truncate)
        (Value::Float32(f), Type::Int8) => Ok(Value::Int8(f as i8)),
        (Value::Float32(f), Type::Int16) => Ok(Value::Int16(f as i16)),
        (Value::Float32(f), Type::Int32) => Ok(Value::Int32(f as i32)),
        (Value::Float32(f), Type::Int64) => Ok(Value::Int64(f as i64)),
        (Value::Float32(f), Type::UInt8) => Ok(Value::UInt8(f as u8)),
        (Value::Float32(f), Type::UInt16) => Ok(Value::UInt16(f as u16)),
        (Value::Float32(f), Type::UInt32) => Ok(Value::UInt32(f as u32)),
        (Value::Float32(f), Type::UInt64) => Ok(Value::UInt64(f as u64)),
        (Value::Float64(f), Type::Int8) => Ok(Value::Int8(f as i8)),
        (Value::Float64(f), Type::Int16) => Ok(Value::Int16(f as i16)),
        (Value::Float64(f), Type::Int32) => Ok(Value::Int32(f as i32)),
        (Value::Float64(f), Type::Int64) => Ok(Value::Int64(f as i64)),
        (Value::Float64(f), Type::UInt8) => Ok(Value::UInt8(f as u8)),
        (Value::Float64(f), Type::UInt16) => Ok(Value::UInt16(f as u16)),
        (Value::Float64(f), Type::UInt32) => Ok(Value::UInt32(f as u32)),
        (Value::Float64(f), Type::UInt64) => Ok(Value::UInt64(f as u64)),

        // Float conversions
        (Value::Float64(f), Type::Float32) => Ok(Value::Float32(f as f32)),
        (Value::Float32(f), Type::Float64) => Ok(Value::Float64(f as f64)),

        // Numeric → Float
        (Value::Int64(i), Type::Float32) => Ok(Value::Float32(i as f32)),
        (Value::Int64(i), Type::Float64) => Ok(Value::Float64(i as f64)),
        (Value::UInt64(u), Type::Float32) => Ok(Value::Float32(u as f32)),
        (Value::UInt64(u), Type::Float64) => Ok(Value::Float64(u as f64)),
        (Value::Int8(i), Type::Float32) => Ok(Value::Float32(i as f32)),
        (Value::Int8(i), Type::Float64) => Ok(Value::Float64(i as f64)),
        (Value::Int16(i), Type::Float32) => Ok(Value::Float32(i as f32)),
        (Value::Int16(i), Type::Float64) => Ok(Value::Float64(i as f64)),
        (Value::Int32(i), Type::Float32) => Ok(Value::Float32(i as f32)),
        (Value::Int32(i), Type::Float64) => Ok(Value::Float64(i as f64)),
        (Value::UInt8(u), Type::Float32) => Ok(Value::Float32(u as f32)),
        (Value::UInt8(u), Type::Float64) => Ok(Value::Float64(u as f64)),
        (Value::UInt16(u), Type::Float32) => Ok(Value::Float32(u as f32)),
        (Value::UInt16(u), Type::Float64) => Ok(Value::Float64(u as f64)),
        (Value::UInt32(u), Type::Float32) => Ok(Value::Float32(u as f32)),
        (Value::UInt32(u), Type::Float64) => Ok(Value::Float64(u as f64)),

        // Convert between smaller integer types
        (Value::Int8(i), Type::UInt8) => Ok(Value::UInt8(i as u8)),
        (Value::Int16(i), Type::UInt16) => Ok(Value::UInt16(i as u16)),
        (Value::Int32(i), Type::UInt32) => Ok(Value::UInt32(i as u32)),
        (Value::UInt8(u), Type::Int8) => Ok(Value::Int8(u as i8)),
        (Value::UInt16(u), Type::Int16) => Ok(Value::Int16(u as i16)),
        (Value::UInt32(u), Type::Int32) => Ok(Value::Int32(u as i32)),

        // String → numeric
        (Value::String(s), Type::Int8) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str
                .parse::<i8>()
                .map(Value::Int8)
                .map_err(|e| Error::SerializeError(format!("Cannot parse '{s_str}' as Int8: {e}")))
        }
        (Value::String(s), Type::Int16) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str
                .parse::<i16>()
                .map(Value::Int16)
                .map_err(|e| Error::SerializeError(format!("Cannot parse '{s_str}' as Int16: {e}")))
        }
        (Value::String(s), Type::Int32) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str
                .parse::<i32>()
                .map(Value::Int32)
                .map_err(|e| Error::SerializeError(format!("Cannot parse '{s_str}' as Int32: {e}")))
        }
        (Value::String(s), Type::Int64) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|e| Error::SerializeError(format!("Cannot parse '{s_str}' as Int64: {e}")))
        }
        (Value::String(s), Type::UInt8) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str
                .parse::<u8>()
                .map(Value::UInt8)
                .map_err(|e| Error::SerializeError(format!("Cannot parse '{s_str}' as UInt8: {e}")))
        }
        (Value::String(s), Type::UInt16) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str.parse::<u16>().map(Value::UInt16).map_err(|e| {
                Error::SerializeError(format!("Cannot parse '{s_str}' as UInt16: {e}"))
            })
        }
        (Value::String(s), Type::UInt32) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str.parse::<u32>().map(Value::UInt32).map_err(|e| {
                Error::SerializeError(format!("Cannot parse '{s_str}' as UInt32: {e}"))
            })
        }
        (Value::String(s), Type::UInt64) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str.parse::<u64>().map(Value::UInt64).map_err(|e| {
                Error::SerializeError(format!("Cannot parse '{s_str}' as UInt64: {e}"))
            })
        }
        (Value::String(s), Type::Float32) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str.parse::<f32>().map(Value::Float32).map_err(|e| {
                Error::SerializeError(format!("Cannot parse '{s_str}' as Float32: {e}"))
            })
        }
        (Value::String(s), Type::Float64) => {
            let s_str = String::from_utf8_lossy(&s);
            s_str.parse::<f64>().map(Value::Float64).map_err(|e| {
                Error::SerializeError(format!("Cannot parse '{s_str}' as Float64: {e}"))
            })
        }

        // Date/DateTime from timestamp
        (Value::Int64(i), Type::Date) => Ok(Value::Date(Date(i as u16))),
        (Value::Int64(i), Type::Date32) => Ok(Value::Date32(Date32(i as i32))),
        (Value::Int64(i), Type::DateTime(tz)) => {
            Ok(Value::DateTime(DateTime(tz.clone(), i as u32)))
        }
        (Value::Int64(i), Type::DateTime64(precision, tz)) => {
            Ok(Value::DateTime64(DynDateTime64(tz.clone(), i as u64, *precision)))
        }
        (Value::UInt64(u), Type::Date) => Ok(Value::Date(Date(u as u16))),
        (Value::UInt64(u), Type::Date32) => Ok(Value::Date32(Date32(u as i32))),
        (Value::UInt64(u), Type::DateTime(tz)) => {
            Ok(Value::DateTime(DateTime(tz.clone(), u as u32)))
        }
        (Value::UInt64(u), Type::DateTime64(precision, tz)) => {
            Ok(Value::DateTime64(DynDateTime64(tz.clone(), u, *precision)))
        }

        // Decimal from numeric
        (Value::Int64(i), Type::Decimal32(scale)) => Ok(Value::Decimal32(*scale, i as i32)),
        (Value::Int64(i), Type::Decimal64(scale)) => Ok(Value::Decimal64(*scale, i)),
        (Value::Int64(i), Type::Decimal128(scale)) => Ok(Value::Decimal128(*scale, i as i128)),
        (Value::UInt64(u), Type::Decimal32(scale)) => Ok(Value::Decimal32(*scale, u as i32)),
        (Value::UInt64(u), Type::Decimal64(scale)) => Ok(Value::Decimal64(*scale, u as i64)),
        (Value::UInt64(u), Type::Decimal128(scale)) => Ok(Value::Decimal128(*scale, u as i128)),
        (Value::Float64(f), Type::Decimal32(scale)) => {
            let scaled = f * 10_f64.powi(*scale as i32);
            Ok(Value::Decimal32(*scale, scaled as i32))
        }
        (Value::Float64(f), Type::Decimal64(scale)) => {
            let scaled = f * 10_f64.powi(*scale as i32);
            Ok(Value::Decimal64(*scale, scaled as i64))
        }
        (Value::Float64(f), Type::Decimal128(scale)) => {
            let scaled = f * 10_f64.powi(*scale as i32);
            Ok(Value::Decimal128(*scale, scaled as i128))
        }

        // Array handling
        (Value::Array(elements), Type::Array(target_elem_type)) => {
            let converted: Result<Vec<Value>> =
                elements.into_iter().map(|e| convert_to_type(e, target_elem_type)).collect();
            Ok(Value::Array(converted?))
        }

        // Tuple element-wise conversion
        (Value::Tuple(elements), Type::Tuple(target_types)) => {
            if elements.len() != target_types.len() {
                return Err(Error::SerializeError(format!(
                    "Tuple length mismatch: got {} elements, expected {}",
                    elements.len(),
                    target_types.len()
                )));
            }
            let converted: Result<Vec<Value>> = elements
                .into_iter()
                .zip(target_types.iter())
                .map(|(e, t)| convert_to_type(e, t))
                .collect();
            Ok(Value::Tuple(converted?))
        }

        // Array → Tuple by position
        (Value::Array(elements), Type::Tuple(target_types)) => {
            if elements.len() != target_types.len() {
                return Err(Error::SerializeError(format!(
                    "Cannot convert Array to Tuple: length mismatch ({} != {})",
                    elements.len(),
                    target_types.len()
                )));
            }
            let converted: Result<Vec<Value>> = elements
                .into_iter()
                .zip(target_types.iter())
                .map(|(e, t)| convert_to_type(e, t))
                .collect();
            Ok(Value::Tuple(converted?))
        }

        // Map handling
        (Value::Map(keys, values), Type::Map(target_key_type, target_value_type)) => {
            if keys.len() != values.len() {
                return Err(Error::SerializeError(format!(
                    "Map keys and values length mismatch: {} != {}",
                    keys.len(),
                    values.len()
                )));
            }
            let converted_keys: Result<Vec<Value>> =
                keys.into_iter().map(|k| convert_to_type(k, target_key_type)).collect();
            let converted_values: Result<Vec<Value>> =
                values.into_iter().map(|v| convert_to_type(v, target_value_type)).collect();
            Ok(Value::Map(converted_keys?, converted_values?))
        }

        // Array of pairs → Map
        (Value::Array(elements), Type::Map(target_key_type, target_value_type)) => {
            let mut keys = Vec::with_capacity(elements.len());
            let mut values = Vec::with_capacity(elements.len());
            for elem in elements {
                match elem {
                    Value::Tuple(pair) if pair.len() == 2 => {
                        let mut it = pair.into_iter();
                        let k = it.next().unwrap();
                        let v = it.next().unwrap();
                        keys.push(convert_to_type(k, target_key_type)?);
                        values.push(convert_to_type(v, target_value_type)?);
                    }
                    Value::Array(pair) if pair.len() == 2 => {
                        let mut it = pair.into_iter();
                        let k = it.next().unwrap();
                        let v = it.next().unwrap();
                        keys.push(convert_to_type(k, target_key_type)?);
                        values.push(convert_to_type(v, target_value_type)?);
                    }
                    _ => {
                        return Err(Error::SerializeError(
                            "Cannot convert Array to Map: elements must be Tuple(K,V) or \
                             Array[K,V]"
                                .to_string(),
                        ));
                    }
                }
            }
            Ok(Value::Map(keys, values))
        }

        // Nullable/LC recurse
        (v, Type::Nullable(inner)) => convert_to_type(v, inner),
        (v, Type::LowCardinality(inner)) => convert_to_type(v, inner),

        // Variant: wrap with discriminator after converting to target
        (value, Type::Variant(variant_types)) => {
            let (disc, original_idx) = discriminate(&value, variant_types)?;
            let target_type = &variant_types[original_idx];
            let converted = convert_to_type(value, target_type)?;
            Ok(Value::Variant(disc, Box::new(converted)))
        }

        // Default: no conversion
        (v, _) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminate_orders_by_type_name_and_handles_null() {
        let types = vec![Type::UInt64, Type::String, Type::Int64];

        // Null -> 0xFF
        let (d_null, _idx) = discriminate(&Value::Null, &types).unwrap();
        assert_eq!(d_null, 0xFF);

        // Sorted order: Int64 (0), String (1), UInt64 (2)
        let (d_i64, _) = discriminate(&Value::Int64(1), &types).unwrap();
        assert_eq!(d_i64, 0);
        let (d_str, _) = discriminate(&Value::String(b"a".to_vec()), &types).unwrap();
        assert_eq!(d_str, 1);
        let (d_u64, _) = discriminate(&Value::UInt64(1), &types).unwrap();
        assert_eq!(d_u64, 2);
    }

    #[test]
    fn convert_string_to_int_and_variant_wrap() {
        // String -> Int64
        let v = convert_to_type(Value::String(b"42".to_vec()), &Type::Int64).unwrap();
        assert!(matches!(v, Value::Int64(42)));

        // Wrap into Variant(Int64|String) => disc 0 for Int64
        let ty = Type::Variant(vec![Type::Int64, Type::String]);
        let v = convert_to_type(Value::Int64(5), &ty).unwrap();
        match v {
            Value::Variant(d, inner) => {
                assert_eq!(d, 0);
                assert!(matches!(*inner, Value::Int64(5)));
            }
            _ => panic!("expected Variant"),
        }
    }
}
