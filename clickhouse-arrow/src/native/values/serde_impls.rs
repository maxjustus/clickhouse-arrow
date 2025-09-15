use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

use crate::native::types::Type;
use crate::native::values::Value;

/// Serialize a single `Value` using a schema `&Type`.
///
/// Semantics:
/// - Named tuples serialize as maps (field name -> typed value)
/// - Unnamed tuples serialize as sequences (positional)
/// - Arrays, Nullable handled recursively
/// - Other values delegate to `Value::to_json()` for stable rendering
pub struct Typed<'a> {
    pub v: &'a Value,
    pub t: &'a Type,
}

impl Serialize for Typed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let t = self.t.strip_null();
        match (self.v, t) {
            // Null handling
            (Value::Null, _) => serializer.serialize_none(),
            (v, Type::Nullable(inner)) => Typed { v, t: inner }.serialize(serializer),

            // Arrays
            (Value::Array(items), Type::Array(inner)) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for it in items {
                    seq.serialize_element(&Typed { v: it, t: inner })?;
                }
                seq.end()
            }

            // Named tuples -> object
            (Value::Tuple(values), Type::TupleNamed(fields)) => {
                let cap = core::cmp::min(values.len(), fields.len());
                let mut map = serializer.serialize_map(Some(cap))?;
                for (i, (name, field_ty)) in fields.iter().enumerate() {
                    if let Some(val) = values.get(i) {
                        map.serialize_entry(name, &Typed { v: val, t: field_ty })?;
                    }
                }
                map.end()
            }

            // Positional tuples -> array
            (Value::Tuple(values), Type::Tuple(inner)) => {
                let cap = core::cmp::min(values.len(), inner.len());
                let mut seq = serializer.serialize_seq(Some(cap))?;
                for i in 0..cap {
                    seq.serialize_element(&Typed { v: &values[i], t: &inner[i] })?;
                }
                seq.end()
            }

            // Map fast-path: serialize as JSON object without building an intermediate DOM
            (Value::Map(keys, values), Type::Map(_key_ty, value_ty)) => {
                let mut map = serializer.serialize_map(Some(keys.len()))?;
                for (k, v) in keys.iter().zip(values.iter()) {
                    // Derive string key similar to Value::to_json map rendering
                    let key_str: String = match k {
                        Value::String(bytes) => String::from_utf8_lossy(bytes).to_string(),
                        _ => {
                            // Fallback: convert value to JSON and stringify
                            match k.to_json() {
                                Ok(j) => match j {
                                    serde_json::Value::String(s) => s,
                                    serde_json::Value::Number(n) => n.to_string(),
                                    serde_json::Value::Bool(b) => b.to_string(),
                                    serde_json::Value::Null => "null".to_string(),
                                    other => serde_json::to_string(&other)
                                        .unwrap_or_else(|_| "null".to_string()),
                                },
                                Err(_) => "null".to_string(),
                            }
                        }
                    };
                    map.serialize_entry(&key_str, &Typed { v, t: value_ty })?;
                }
                map.end()
            }

            // Fast-path primitives and common scalars to avoid building serde_json::Value
            (Value::Int8(i), _) => serializer.serialize_i8(*i),
            (Value::Int16(i), _) => serializer.serialize_i16(*i),
            (Value::Int32(i), _) => serializer.serialize_i32(*i),
            (Value::Int64(i), _) => serializer.serialize_i64(*i),
            (Value::UInt8(i), _) => serializer.serialize_u8(*i),
            (Value::UInt16(i), _) => serializer.serialize_u16(*i),
            (Value::UInt32(i), _) => serializer.serialize_u32(*i),
            (Value::UInt64(i), _) => serializer.serialize_u64(*i),
            // Large integers as strings for JSON compatibility
            (Value::Int128(i), _) => serializer.serialize_str(&i.to_string()),
            (Value::UInt128(i), _) => serializer.serialize_str(&i.to_string()),
            (Value::Int256(i), _) => serializer.serialize_str(&i.to_string()),
            (Value::UInt256(i), _) => serializer.serialize_str(&i.to_string()),
            // Floats: non-finite map to null (matches to_json fallback)
            (Value::Float32(f), _) => match serde_json::Number::from_f64(f64::from(*f)) {
                Some(_) => serializer.serialize_f32(*f),
                None => serializer.serialize_none(),
            },
            (Value::Float64(f), _) => match serde_json::Number::from_f64(*f) {
                Some(_) => serializer.serialize_f64(*f),
                None => serializer.serialize_none(),
            },
            // Decimals: render as strings with decimal point
            (Value::Decimal32(scale, v), _) => {
                serializer.serialize_str(&super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal64(scale, v), _) => {
                serializer.serialize_str(&super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal128(scale, v), _) => {
                serializer.serialize_str(&super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal256(scale, v), _) => {
                serializer.serialize_str(&super::format_decimal(v.to_string(), *scale))
            }
            // Strings may be non-UTF8 (FixedString/Binary); use lossy for stability
            (Value::String(bytes), _) => serializer.serialize_str(&String::from_utf8_lossy(bytes)),
            // UUID / IP as canonical strings
            (Value::Uuid(u), _) => serializer.serialize_str(&u.to_string()),
            (Value::Ipv4(ip), _) => serializer.serialize_str(&ip.to_string()),
            (Value::Ipv6(ip), _) => serializer.serialize_str(&ip.to_string()),
            // Enums: serialize the textual variant like ClickHouse JSONEachRow
            (Value::Enum8(name, _), _) | (Value::Enum16(name, _), _) => {
                serializer.serialize_str(name)
            }

            // TODO: bench just doing this instead of the fast path crap
            // Fallback to existing JSON-compatible rendering
            (v, _) => match v.to_json() {
                Ok(json) => json.serialize(serializer),
                Err(e) => Err(serde::ser::Error::custom(e.to_string())),
            },
        }
    }
}

/// Serialize a row object from `(name, Type)` columns and corresponding `Value`s.
pub struct RowSer<'a> {
    pub cols: &'a [(String, Type)],
    pub row:  &'a [Value],
}

impl Serialize for RowSer<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let cap = core::cmp::min(self.cols.len(), self.row.len());
        let mut map = serializer.serialize_map(Some(cap))?;
        for i in 0..cap {
            let (ref name, ref ty) = self.cols[i];
            map.serialize_entry(name, &Typed { v: &self.row[i], t: ty })?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn named_tuple_serializes_as_object() {
        let ty = Type::TupleNamed(vec![("a".into(), Type::String), ("b".into(), Type::Int64)]);
        let v = Value::Tuple(vec![Value::string("x"), Value::Int64(7)]);
        let got = serde_json::to_value(Typed { v: &v, t: &ty }).unwrap();
        assert_eq!(got, json!({"a": "x", "b": 7}));
    }

    #[test]
    fn array_of_named_tuple_serializes_as_array_of_objects() {
        let ty = Type::Array(Box::new(Type::TupleNamed(vec![
            ("a".into(), Type::String),
            ("b".into(), Type::Int64),
        ])));
        let v = Value::Array(vec![
            Value::Tuple(vec![Value::string("x"), Value::Int64(7)]),
            Value::Tuple(vec![Value::string("y"), Value::Int64(8)]),
        ]);
        let got = serde_json::to_value(Typed { v: &v, t: &ty }).unwrap();
        assert_eq!(got, json!([{"a": "x", "b": 7}, {"a": "y", "b": 8}]));
    }

    #[test]
    fn rowser_serializes_row_object() {
        let cols = vec![
            (
                "t".into(),
                Type::TupleNamed(vec![("a".into(), Type::String), ("b".into(), Type::Int64)]),
            ),
            ("n".into(), Type::Nullable(Box::new(Type::Int32))),
        ];
        let row = vec![Value::Tuple(vec![Value::string("z"), Value::Int64(1)]), Value::Null];
        let got = serde_json::to_value(RowSer { cols: &cols, row: &row }).unwrap();
        assert_eq!(got, json!({"t": {"a": "z", "b": 1}, "n": null}));
    }
}
