use ::serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::native::convert::FromSql;
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
        serialize_typed_impl(self.v, self.t, MapKeyPolicy::Stringify, serializer)
    }
}

fn serialize_typed_impl<S: Serializer>(
    v: &Value,
    t: &Type,
    map_key_policy: MapKeyPolicy,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let t = t.strip_null();
    match (v, t) {
        // Null handling
        (Value::Null, _) => serializer.serialize_none(),
        (v, Type::Nullable(inner)) => serialize_typed_impl(v, inner, map_key_policy, serializer),

        // Arrays
        (Value::Array(items), Type::Array(inner)) => {
            let mut seq = serializer.serialize_seq(Some(items.len()))?;
            for it in items {
                seq.serialize_element(&TypedWith { v: it, t: inner, map_key_policy })?;
            }
            seq.end()
        }

        // Named tuples -> object
        (Value::Tuple(values), Type::TupleNamed(fields)) => {
            let cap = core::cmp::min(values.len(), fields.len());
            let mut map = serializer.serialize_map(Some(cap))?;
            for (i, (name, field_ty)) in fields.iter().enumerate() {
                if let Some(val) = values.get(i) {
                    map.serialize_entry(name, &TypedWith { v: val, t: field_ty, map_key_policy })?;
                }
            }
            map.end()
        }

        // Positional tuples -> array
        (Value::Tuple(values), Type::Tuple(inner)) => {
            let cap = core::cmp::min(values.len(), inner.len());
            let mut seq = serializer.serialize_seq(Some(cap))?;
            for i in 0..cap {
                seq.serialize_element(&TypedWith { v: &values[i], t: &inner[i], map_key_policy })?;
            }
            seq.end()
        }

        // Map fast-path: serialize as JSON object without building an intermediate DOM
        (Value::Map(keys, values), Type::Map(key_ty, value_ty)) => match map_key_policy {
            MapKeyPolicy::Stringify => {
                let mut map = serializer.serialize_map(Some(keys.len()))?;
                for (k, v) in keys.iter().zip(values.iter()) {
                    let key_str = stringify_key_for_json(k);
                    map.serialize_entry(&key_str, &TypedWith { v, t: value_ty, map_key_policy })?;
                }
                map.end()
            }
            MapKeyPolicy::Native => {
                struct Key<'a> {
                    k: &'a Value,
                    t: &'a Type,
                    p: MapKeyPolicy,
                }
                impl Serialize for Key<'_> {
                    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        serialize_typed_impl(self.k, self.t, self.p, serializer)
                    }
                }
                let mut map = serializer.serialize_map(Some(keys.len()))?;
                for (k, v) in keys.iter().zip(values.iter()) {
                    map.serialize_entry(&Key { k, t: key_ty, p: map_key_policy }, &TypedWith {
                        v,
                        t: value_ty,
                        map_key_policy,
                    })?;
                }
                map.end()
            }
            MapKeyPolicy::Pairs => {
                struct PairSer<'a> {
                    k:  &'a Value,
                    kt: &'a Type,
                    v:  &'a Value,
                    vt: &'a Type,
                    p:  MapKeyPolicy,
                }
                impl Serialize for PairSer<'_> {
                    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        let mut pair = serializer.serialize_seq(Some(2))?;
                        pair.serialize_element(&TypedWith {
                            v:              self.k,
                            t:              self.kt,
                            map_key_policy: self.p,
                        })?;
                        pair.serialize_element(&TypedWith {
                            v:              self.v,
                            t:              self.vt,
                            map_key_policy: self.p,
                        })?;
                        pair.end()
                    }
                }
                let mut outer = serializer.serialize_seq(Some(keys.len()))?;
                for (k, v) in keys.iter().zip(values.iter()) {
                    outer.serialize_element(&PairSer {
                        k,
                        kt: key_ty,
                        v,
                        vt: value_ty,
                        p: map_key_policy,
                    })?;
                }
                outer.end()
            }
        },

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
            serializer.serialize_str(&super::super::format_decimal(v.to_string(), *scale))
        }
        (Value::Decimal64(scale, v), _) => {
            serializer.serialize_str(&super::super::format_decimal(v.to_string(), *scale))
        }
        (Value::Decimal128(scale, v), _) => {
            serializer.serialize_str(&super::super::format_decimal(v.to_string(), *scale))
        }
        (Value::Decimal256(scale, v), _) => {
            serializer.serialize_str(&super::super::format_decimal(v.to_string(), *scale))
        }
        // Strings may be non-UTF8 (FixedString/Binary); use lossy for stability
        (Value::String(bytes), _) => serializer.serialize_str(&String::from_utf8_lossy(bytes)),
        // UUID / IP as canonical strings
        (Value::Uuid(u), _) => serializer.serialize_str(&u.to_string()),
        (Value::Ipv4(ip), _) => serializer.serialize_str(&ip.to_string()),
        (Value::Ipv6(ip), _) => serializer.serialize_str(&ip.to_string()),
        // Enums: serialize the textual variant like ClickHouse JSONEachRow
        (Value::Enum8(name, _) | Value::Enum16(name, _), _) => serializer.serialize_str(name),

        // Dates: ISO date strings
        (Value::Date(date), _) => {
            let d: NaiveDate = (*date).into();
            serializer.serialize_str(&d.format("%Y-%m-%d").to_string())
        }
        (Value::Date32(date), _) => {
            let d: NaiveDate = (*date).into();
            serializer.serialize_str(&d.format("%Y-%m-%d").to_string())
        }
        // DateTime: "YYYY-MM-DD HH:MM:SS" (ClickHouse-style)
        (Value::DateTime(datetime), _) => {
            let ch: chrono::DateTime<Tz> = (*datetime)
                .try_into()
                .map_err(|_| ::serde::ser::Error::custom("Invalid DateTime"))?;
            serializer.serialize_str(&ch.format("%Y-%m-%d %H:%M:%S").to_string())
        }
        // DateTime64 with precision handling like Value::to_json
        (Value::DateTime64(datetime), _) => {
            let ty = Type::DateTime64(datetime.2, datetime.0);
            let v = Value::DateTime64(*datetime);
            let ch: chrono::DateTime<Tz> = FromSql::from_sql(&ty, v)
                .map_err(|e| ::serde::ser::Error::custom(format!("Invalid DateTime64: {e}")))?;
            let formatted = match datetime.2 {
                0 => ch.format("%Y-%m-%d %H:%M:%S").to_string(),
                3 => ch.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
                6 => ch.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
                9 => ch.format("%Y-%m-%d %H:%M:%S%.9f").to_string(),
                other => {
                    // General case: derive subseconds width
                    let nanos = ch.timestamp_subsec_nanos();
                    let divisor = 10_u32.saturating_pow(9_u32.saturating_sub(other as u32));
                    let subsec = if divisor == 0 { nanos } else { nanos / divisor };
                    format!("{}.{:0width$}", ch.format("%Y-%m-%d %H:%M:%S"), subsec, width = other)
                }
            };
            serializer.serialize_str(&formatted)
        }

        // Variant/Dynamic: unwrap and serialize contained value
        (Value::Variant(_, boxed) | Value::Dynamic(_, boxed), _) => {
            let inner_ty = boxed.guess_type();
            serialize_typed_impl(boxed, &inner_ty, map_key_policy, serializer)
        }

        // Object: parse as JSON and serialize that
        (Value::Object(bytes), _) => match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(json) => json.serialize(serializer),
            Err(e) => Err(::serde::ser::Error::custom(format!("Invalid JSON in Object: {e}"))),
        },
        // Json: pass-through
        #[cfg(feature = "serde")]
        (Value::Json(json), _) => json.serialize(serializer),

        // Geo types: serialize as nested sequences without reusing the outer serializer
        (Value::Point(point), _) => {
            struct PointSer(f64, f64);
            impl Serialize for PointSer {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    let mut seq = serializer.serialize_seq(Some(2))?;
                    seq.serialize_element(&self.0)?;
                    seq.serialize_element(&self.1)?;
                    seq.end()
                }
            }
            PointSer(point.0[0], point.0[1]).serialize(serializer)
        }
        (Value::Ring(ring), _) => {
            struct PointSer(f64, f64);
            impl Serialize for PointSer {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    let mut seq = serializer.serialize_seq(Some(2))?;
                    seq.serialize_element(&self.0)?;
                    seq.serialize_element(&self.1)?;
                    seq.end()
                }
            }
            let mut outer = serializer.serialize_seq(Some(ring.0.len()))?;
            for p in &ring.0 {
                outer.serialize_element(&PointSer(p.0[0], p.0[1]))?;
            }
            outer.end()
        }
        (Value::Polygon(polygon), _) => {
            struct RingSer<'a>(&'a crate::native::values::Ring);
            impl Serialize for RingSer<'_> {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    struct PointSer(f64, f64);
                    impl Serialize for PointSer {
                        fn serialize<S: Serializer>(
                            &self,
                            serializer: S,
                        ) -> Result<S::Ok, S::Error> {
                            let mut seq = serializer.serialize_seq(Some(2))?;
                            seq.serialize_element(&self.0)?;
                            seq.serialize_element(&self.1)?;
                            seq.end()
                        }
                    }
                    let mut seq = serializer.serialize_seq(Some(self.0.0.len()))?;
                    for p in &self.0.0 {
                        seq.serialize_element(&PointSer(p.0[0], p.0[1]))?;
                    }
                    seq.end()
                }
            }
            let mut outer = serializer.serialize_seq(Some(polygon.0.len()))?;
            for ring in &polygon.0 {
                outer.serialize_element(&RingSer(ring))?;
            }
            outer.end()
        }
        (Value::MultiPolygon(multi), _) => {
            struct RingSer<'a>(&'a crate::native::values::Ring);
            impl Serialize for RingSer<'_> {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    struct PointSer(f64, f64);
                    impl Serialize for PointSer {
                        fn serialize<S: Serializer>(
                            &self,
                            serializer: S,
                        ) -> Result<S::Ok, S::Error> {
                            let mut seq = serializer.serialize_seq(Some(2))?;
                            seq.serialize_element(&self.0)?;
                            seq.serialize_element(&self.1)?;
                            seq.end()
                        }
                    }
                    let mut seq = serializer.serialize_seq(Some(self.0.0.len()))?;
                    for p in &self.0.0 {
                        seq.serialize_element(&PointSer(p.0[0], p.0[1]))?;
                    }
                    seq.end()
                }
            }
            struct PolySer<'a>(&'a crate::native::values::Polygon);
            impl Serialize for PolySer<'_> {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    let mut seq = serializer.serialize_seq(Some(self.0.0.len()))?;
                    for ring in &self.0.0 {
                        seq.serialize_element(&RingSer(ring))?;
                    }
                    seq.end()
                }
            }
            let mut outer = serializer.serialize_seq(Some(multi.0.len()))?;
            for poly in &multi.0 {
                outer.serialize_element(&PolySer(poly))?;
            }
            outer.end()
        }
        // Fallback: if provided schema doesn't align with value, serialize value via its JSON
        // rendering and delegate to the serializer. `to_json()` now routes through the
        // typed path with `guess_type()`, so this converges without infinite recursion.
        (v, _) => match v.to_json() {
            Ok(json) => json.serialize(serializer),
            Err(e) => Err(::serde::ser::Error::custom(e.to_string())),
        },
    }
}

/// Policy for serializing map keys.
///
/// - Stringify: always render keys as strings (JSON-compatible; current default behavior).
/// - Native: serialize keys via their native types (works for CBOR/MsgPack; JSON backends will
///   error).
/// - Pairs: encode maps as `[[key, value], ...]` pairs; useful for backends with restricted keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapKeyPolicy {
    Stringify,
    Native,
    Pairs,
}

impl Default for MapKeyPolicy {
    fn default() -> Self { MapKeyPolicy::Stringify }
}

/// Serialize a `Value` with an explicit map-key policy.
pub struct TypedWith<'a> {
    pub v:              &'a Value,
    pub t:              &'a Type,
    pub map_key_policy: MapKeyPolicy,
}

impl Serialize for TypedWith<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_typed_impl(self.v, self.t, self.map_key_policy, serializer)
    }
}

pub(super) fn stringify_key_for_json(k: &Value) -> String {
    match k {
        Value::String(bytes) => String::from_utf8_lossy(bytes).to_string(),
        _ => match k.to_json() {
            Ok(j) => match j {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Null => "null".to_string(),
                other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
            },
            Err(_) => "null".to_string(),
        },
    }
}

/// Serialize a row object from `(name, Type)` columns and corresponding `Value`s.
pub struct RowSerializer<'a> {
    pub cols: &'a [(String, Type)],
    pub row:  &'a [Value],
}

impl Serialize for RowSerializer<'_> {
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

/// Serialize a row object but render TupleNamed cells as arrays (positional),
/// to maintain legacy JSON shape for sources like Object('json') that materialize
/// into named tuples server-side.
/// Serialize a row object with an explicit map-key policy.
pub struct RowSerWith<'a> {
    pub cols:           &'a [(String, Type)],
    pub row:            &'a [Value],
    pub map_key_policy: MapKeyPolicy,
}

impl Serialize for RowSerWith<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let cap = core::cmp::min(self.cols.len(), self.row.len());
        let mut map = serializer.serialize_map(Some(cap))?;
        for i in 0..cap {
            let (ref name, ref ty) = self.cols[i];
            map.serialize_entry(name, &TypedWith {
                v:              &self.row[i],
                t:              ty,
                map_key_policy: self.map_key_policy,
            })?;
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
        let got = serde_json::to_value(RowSerializer { cols: &cols, row: &row }).unwrap();
        assert_eq!(got, json!({"t": {"a": "z", "b": 1}, "n": null}));
    }

    #[test]
    fn map_stringify_policy_serializes_string_keys() {
        let ty = Type::Map(Box::new(Type::Int64), Box::new(Type::String));
        let v = Value::Map(vec![Value::Int64(7), Value::Int64(42)], vec![
            Value::string("x"),
            Value::string("y"),
        ]);
        let got = serde_json::to_value(TypedWith {
            v:              &v,
            t:              &ty,
            map_key_policy: MapKeyPolicy::Stringify,
        })
        .unwrap();
        assert_eq!(got, json!({"7": "x", "42": "y"}));
    }

    #[test]
    fn map_native_policy_on_json_backend_coerces_to_strings() {
        let ty = Type::Map(Box::new(Type::Int64), Box::new(Type::String));
        let v = Value::Map(vec![Value::Int64(7)], vec![Value::string("x")]);
        // serde_json coerces non-string keys to strings when serializing maps
        let got = serde_json::to_value(TypedWith {
            v:              &v,
            t:              &ty,
            map_key_policy: MapKeyPolicy::Native,
        })
        .unwrap();
        assert_eq!(got, json!({"7": "x"}));
    }

    #[test]
    fn map_native_policy_tuple_key_errors_on_json_backend() {
        // Composite key (tuple) cannot be coerced to a JSON object key; serde_json should error
        let key_ty = Type::Tuple(vec![Type::Int32, Type::Int32]);
        let ty = Type::Map(Box::new(key_ty.clone()), Box::new(Type::String));
        let key = Value::Tuple(vec![Value::Int32(1), Value::Int32(2)]);
        let v = Value::Map(vec![key], vec![Value::string("x")]);
        let err = serde_json::to_value(TypedWith {
            v:              &v,
            t:              &ty,
            map_key_policy: MapKeyPolicy::Native,
        })
        .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("key") || msg.contains("string"));
    }

    #[test]
    fn typed_matches_to_json_for_scalars_and_dates() {
        // Decimal string formatting
        let v = Value::Decimal64(2, 1234);
        let t = Type::Decimal64(2);
        let got = serde_json::to_value(Typed { v: &v, t: &t }).unwrap();
        assert_eq!(got, v.to_json().unwrap());

        // Date
        let v = Value::Date(crate::Date(0));
        let t = Type::Date;
        let got = serde_json::to_value(Typed { v: &v, t: &t }).unwrap();
        assert_eq!(got, v.to_json().unwrap());

        // DateTime64 with scale 3
        let tz = chrono_tz::UTC;
        let v = Value::DateTime64(crate::DynDateTime64(tz, 1_700_000_000_000, 3));
        let t = Type::DateTime64(3, tz);
        let got = serde_json::to_value(Typed { v: &v, t: &t }).unwrap();
        assert_eq!(got, v.to_json().unwrap());
    }

    #[test]
    fn typed_matches_to_json_for_object_and_geo() {
        // Object (JSON bytes)
        let obj = serde_json::json!({"k": [1,2,3], "b": true});
        let bytes = serde_json::to_vec(&obj).unwrap();
        let v = Value::Object(bytes);
        let t = Type::Object;
        let got = serde_json::to_value(Typed { v: &v, t: &t }).unwrap();
        assert_eq!(got, v.to_json().unwrap());

        // Geo: Point
        let v = Value::Point(crate::Point([1.0, 2.0]));
        let t = Type::Point;
        let got = serde_json::to_value(Typed { v: &v, t: &t }).unwrap();
        assert_eq!(got, v.to_json().unwrap());
    }
}
