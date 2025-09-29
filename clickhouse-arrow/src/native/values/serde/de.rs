use ::serde::Deserializer;
use ::serde::de::value::SeqAccessDeserializer;
use ::serde::de::{
    self, DeserializeSeed, Error as DeError, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};
use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::native::convert::FromSql;
use crate::native::types::Type;
use crate::native::values::serde::ser::stringify_key_for_json;
use crate::native::values::{Point, Polygon, Ring, Value};

// Macro to generate deserialize_* methods that forward to deserialize_any
// This is a standard serde pattern to reduce boilerplate
macro_rules! forward_to_deserialize_any {
    ($($method:ident),*) => {
        $(
            fn $method<V>(self, visitor: V) -> Result<V::Value, Self::Error>
            where
                V: Visitor<'de>,
            {
                self.deserialize_any(visitor)
            }
        )*
    };
}

struct PointAccess {
    point: Point,
    idx: usize,
}

impl<'de> SeqAccess<'de> for PointAccess {
    type Error = serde_json::Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.idx >= 2 {
            return Ok(None);
        }
        let coord = self.point.0[self.idx];
        self.idx += 1;
        seed.deserialize(coord.into_deserializer()).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(2_usize.saturating_sub(self.idx))
    }
}

struct RingAccess<'a> {
    points: &'a [Point],
    idx: usize,
}

impl<'de> SeqAccess<'de> for RingAccess<'_> {
    type Error = serde_json::Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.idx >= self.points.len() {
            return Ok(None);
        }
        let point = self.points[self.idx];
        self.idx += 1;
        let seq = SeqAccessDeserializer::new(PointAccess { point, idx: 0 });
        seed.deserialize(seq).map(Some).map_err(|err| DeError::custom(err))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.points.len().saturating_sub(self.idx))
    }
}

struct PolygonAccess<'a> {
    rings: &'a [Ring],
    idx: usize,
}

impl<'de> SeqAccess<'de> for PolygonAccess<'_> {
    type Error = serde_json::Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.idx >= self.rings.len() {
            return Ok(None);
        }
        let ring = &self.rings[self.idx];
        self.idx += 1;
        let seq = SeqAccessDeserializer::new(RingAccess { points: &ring.0, idx: 0 });
        seed.deserialize(seq).map(Some).map_err(|err| DeError::custom(err))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.rings.len().saturating_sub(self.idx))
    }
}

struct MultiPolygonAccess<'a> {
    polygons: &'a [Polygon],
    idx: usize,
}

impl<'de> SeqAccess<'de> for MultiPolygonAccess<'_> {
    type Error = serde_json::Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.idx >= self.polygons.len() {
            return Ok(None);
        }
        let polygon = &self.polygons[self.idx];
        self.idx += 1;
        let seq = SeqAccessDeserializer::new(PolygonAccess { rings: &polygon.0, idx: 0 });
        seed.deserialize(seq).map(Some).map_err(|err| DeError::custom(err))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.polygons.len().saturating_sub(self.idx))
    }
}

/// A type-aware deserializer over a `Value` guided by a `Type` schema.
pub struct TypedDeserializer<'a> {
    pub v: &'a Value,
    pub t: &'a Type,
}

impl<'de> Deserializer<'de> for TypedDeserializer<'_> {
    type Error = serde_json::Error;

    // Delegate primitive type methods to deserialize_any
    forward_to_deserialize_any! {
        deserialize_bool,
        deserialize_i8, deserialize_i16, deserialize_i32, deserialize_i64,
        deserialize_u8, deserialize_u16, deserialize_u32, deserialize_u64,
        deserialize_f32, deserialize_f64,
        deserialize_char, deserialize_str, deserialize_string,
        deserialize_bytes, deserialize_byte_buf,
        deserialize_identifier, deserialize_ignored_any
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if matches!(self.v, Value::Null) {
            return visitor.visit_none();
        }

        let mut ty = self.t;
        loop {
            match ty {
                Type::Nullable(inner) | Type::LowCardinality(inner) => {
                    ty = inner;
                }
                _ => break,
            }
        }

        match (self.v, ty) {
            (Value::Array(items), Type::Array(inner)) => {
                struct ArrAccess<'a> {
                    items: &'a [Value],
                    idx: usize,
                    inner: &'a Type,
                }
                impl<'de> SeqAccess<'de> for ArrAccess<'_> {
                    type Error = serde_json::Error;

                    fn next_element_seed<T>(
                        &mut self,
                        seed: T,
                    ) -> Result<Option<T::Value>, Self::Error>
                    where
                        T: DeserializeSeed<'de>,
                    {
                        if self.idx >= self.items.len() {
                            return Ok(None);
                        }
                        let i = self.idx;
                        self.idx += 1;
                        let de = TypedDeserializer { v: &self.items[i], t: self.inner };
                        seed.deserialize(de).map(Some)
                    }
                }
                visitor.visit_seq(ArrAccess { items, idx: 0, inner })
            }

            (Value::Tuple(values), Type::TupleNamed(fields)) => {
                let cap = core::cmp::min(values.len(), fields.len());
                struct NTAccess<'a> {
                    values: &'a [Value],
                    fields: &'a [(String, Type)],
                    idx: usize,
                    cap: usize,
                }
                impl<'de> MapAccess<'de> for NTAccess<'_> {
                    type Error = serde_json::Error;

                    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
                    where
                        K: DeserializeSeed<'de>,
                    {
                        if self.idx >= self.cap {
                            return Ok(None);
                        }
                        let key: &str = &self.fields[self.idx].0;
                        seed.deserialize(key.into_deserializer()).map(Some)
                    }

                    fn next_value_seed<VV>(&mut self, seed: VV) -> Result<VV::Value, Self::Error>
                    where
                        VV: DeserializeSeed<'de>,
                    {
                        let i = self.idx;
                        self.idx += 1;
                        let (_, ref ty) = self.fields[i];
                        let val = &self.values[i];
                        let de = TypedDeserializer { v: val, t: ty };
                        seed.deserialize(de)
                    }
                }
                visitor.visit_map(NTAccess { values, fields, idx: 0, cap })
            }

            (Value::Tuple(values), Type::Tuple(inner)) => {
                let cap = core::cmp::min(values.len(), inner.len());
                struct TupAccess<'a> {
                    values: &'a [Value],
                    types: &'a [Type],
                    idx: usize,
                    cap: usize,
                }
                impl<'de> SeqAccess<'de> for TupAccess<'_> {
                    type Error = serde_json::Error;

                    fn next_element_seed<T>(
                        &mut self,
                        seed: T,
                    ) -> Result<Option<T::Value>, Self::Error>
                    where
                        T: DeserializeSeed<'de>,
                    {
                        if self.idx >= self.cap {
                            return Ok(None);
                        }
                        let i = self.idx;
                        self.idx += 1;
                        let de = TypedDeserializer { v: &self.values[i], t: &self.types[i] };
                        seed.deserialize(de).map(Some)
                    }
                }
                visitor.visit_seq(TupAccess { values, types: inner, idx: 0, cap })
            }

            (Value::Map(keys, values), Type::Map(_key_ty, value_ty)) => {
                let cap = core::cmp::min(keys.len(), values.len());
                struct MapAccessImpl<'a> {
                    keys: &'a [Value],
                    values: &'a [Value],
                    value_ty: &'a Type,
                    idx: usize,
                    cap: usize,
                }
                impl<'de> MapAccess<'de> for MapAccessImpl<'_> {
                    type Error = serde_json::Error;

                    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
                    where
                        K: DeserializeSeed<'de>,
                    {
                        if self.idx >= self.cap {
                            return Ok(None);
                        }
                        let key = stringify_key_for_json(&self.keys[self.idx]);
                        seed.deserialize(key.into_deserializer()).map(Some)
                    }

                    fn next_value_seed<VV>(&mut self, seed: VV) -> Result<VV::Value, Self::Error>
                    where
                        VV: DeserializeSeed<'de>,
                    {
                        let i = self.idx;
                        self.idx += 1;
                        let de = TypedDeserializer { v: &self.values[i], t: self.value_ty };
                        seed.deserialize(de)
                    }
                }
                visitor.visit_map(MapAccessImpl { keys, values, value_ty, idx: 0, cap })
            }

            (Value::Int8(i), _) => visitor.visit_i8(*i),
            (Value::Int16(i), _) => visitor.visit_i16(*i),
            (Value::Int32(i), _) => visitor.visit_i32(*i),
            (Value::Int64(i), _) => visitor.visit_i64(*i),
            (Value::UInt8(i), _) => visitor.visit_u8(*i),
            (Value::UInt16(i), _) => visitor.visit_u16(*i),
            (Value::UInt32(i), _) => visitor.visit_u32(*i),
            (Value::UInt64(i), _) => visitor.visit_u64(*i),
            (Value::Int128(i), _) => visitor.visit_string(i.to_string()),
            (Value::UInt128(i), _) => visitor.visit_string(i.to_string()),
            (Value::Int256(i), _) => visitor.visit_string(i.to_string()),
            (Value::UInt256(i), _) => visitor.visit_string(i.to_string()),

            (Value::Float32(f), _) => {
                if f.is_finite() {
                    visitor.visit_f32(*f)
                } else {
                    visitor.visit_none()
                }
            }
            (Value::Float64(f), _) => {
                if f.is_finite() {
                    visitor.visit_f64(*f)
                } else {
                    visitor.visit_none()
                }
            }

            (Value::Decimal32(scale, v), _) => {
                visitor.visit_string(super::super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal64(scale, v), _) => {
                visitor.visit_string(super::super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal128(scale, v), _) => {
                visitor.visit_string(super::super::format_decimal(v.to_string(), *scale))
            }
            (Value::Decimal256(scale, v), _) => {
                visitor.visit_string(super::super::format_decimal(v.to_string(), *scale))
            }

            (Value::String(bytes), _) => {
                visitor.visit_string(String::from_utf8_lossy(bytes).into_owned())
            }
            (Value::Uuid(u), _) => visitor.visit_string(u.to_string()),
            (Value::Ipv4(ip), _) => visitor.visit_string(ip.to_string()),
            (Value::Ipv6(ip), _) => visitor.visit_string(ip.to_string()),
            (Value::Enum8(name, _), _) | (Value::Enum16(name, _), _) => {
                visitor.visit_string(name.clone())
            }

            (Value::Date(date), _) => {
                let d: NaiveDate = (*date).into();
                visitor.visit_string(d.format("%Y-%m-%d").to_string())
            }
            (Value::Date32(date), _) => {
                let d: NaiveDate = (*date).into();
                visitor.visit_string(d.format("%Y-%m-%d").to_string())
            }
            (Value::DateTime(datetime), Type::DateTime(_tz)) => {
                let ch: chrono::DateTime<Tz> =
                    (*datetime).try_into().map_err(|_| de::Error::custom("Invalid DateTime"))?;
                visitor.visit_string(ch.format("%Y-%m-%d %H:%M:%S").to_string())
            }
            (Value::DateTime64(datetime), Type::DateTime64(scale, tz)) => {
                let ty = Type::DateTime64(*scale, *tz);
                let value = Value::DateTime64(*datetime);
                let ch: chrono::DateTime<Tz> = FromSql::from_sql(&ty, value)
                    .map_err(|e| de::Error::custom(format!("Invalid DateTime64: {e}")))?;
                let formatted = match *scale {
                    0 => ch.format("%Y-%m-%d %H:%M:%S").to_string(),
                    3 => ch.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
                    6 => ch.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
                    9 => ch.format("%Y-%m-%d %H:%M:%S%.9f").to_string(),
                    other => {
                        let nanos = ch.timestamp_subsec_nanos();
                        let divisor = 10_u32.saturating_pow(9_u32.saturating_sub(other as u32));
                        let subsec = if divisor == 0 { nanos } else { nanos / divisor };
                        format!(
                            "{}.{subsec:0width$}",
                            ch.format("%Y-%m-%d %H:%M:%S"),
                            width = other
                        )
                    }
                };
                visitor.visit_string(formatted)
            }

            (Value::Point(point), Type::Point) => {
                visitor.visit_seq(PointAccess { point: *point, idx: 0 })
            }
            (Value::Ring(ring), Type::Ring) => {
                visitor.visit_seq(RingAccess { points: &ring.0, idx: 0 })
            }
            (Value::Polygon(polygon), Type::Polygon) => {
                visitor.visit_seq(PolygonAccess { rings: &polygon.0, idx: 0 })
            }
            (Value::MultiPolygon(multi), Type::MultiPolygon) => {
                visitor.visit_seq(MultiPolygonAccess { polygons: &multi.0, idx: 0 })
            }

            (Value::Variant(_, inner), _) => {
                let inner_ty = inner.guess_type();
                TypedDeserializer { v: inner, t: &inner_ty }.deserialize_any(visitor)
            }
            (Value::Dynamic(_, inner), _) => {
                let inner_ty = inner.guess_type();
                TypedDeserializer { v: inner, t: &inner_ty }.deserialize_any(visitor)
            }
            (Value::Json(json), _) => json.clone().into_deserializer().deserialize_any(visitor),
            (Value::Object(bytes), _) => {
                let json: serde_json::Value = serde_json::from_slice(bytes)
                    .map_err(|e| de::Error::custom(format!("Invalid JSON in Object: {e}")))?;
                json.into_deserializer().deserialize_any(visitor)
            }

            // Fallback: delegate to JSON rendering for primitives/others
            (v, _) => {
                let json =
                    v.to_json().map_err(|e| de::Error::custom(format!("to_json error: {e}")))?;
                // Manually route based on JSON value to keep error type consistent
                match json {
                    serde_json::Value::Null => visitor.visit_none(),
                    serde_json::Value::Bool(b) => visitor.visit_bool(b),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            visitor.visit_i64(i)
                        } else if let Some(u) = n.as_u64() {
                            visitor.visit_u64(u)
                        } else if let Some(f) = n.as_f64() {
                            visitor.visit_f64(f)
                        } else {
                            Err(de::Error::custom("invalid JSON number"))
                        }
                    }
                    serde_json::Value::String(s) => visitor.visit_string(s),
                    serde_json::Value::Array(arr) => {
                        struct JsonArrAccess {
                            items: std::vec::IntoIter<serde_json::Value>,
                        }
                        impl<'de> SeqAccess<'de> for JsonArrAccess {
                            type Error = serde_json::Error;

                            fn next_element_seed<T>(
                                &mut self,
                                seed: T,
                            ) -> Result<Option<T::Value>, Self::Error>
                            where
                                T: DeserializeSeed<'de>,
                            {
                                if let Some(v) = self.items.next() {
                                    let de = v.into_deserializer();
                                    seed.deserialize(de).map(Some)
                                } else {
                                    Ok(None)
                                }
                            }
                        }
                        visitor.visit_seq(JsonArrAccess { items: arr.into_iter() })
                    }
                    serde_json::Value::Object(obj) => {
                        struct JsonMapAccess {
                            iter: std::collections::btree_map::IntoIter<String, serde_json::Value>,
                            cur: Option<(String, serde_json::Value)>,
                        }
                        impl<'de> MapAccess<'de> for JsonMapAccess {
                            type Error = serde_json::Error;

                            fn next_key_seed<K>(
                                &mut self,
                                seed: K,
                            ) -> Result<Option<K::Value>, Self::Error>
                            where
                                K: DeserializeSeed<'de>,
                            {
                                if let Some((k, v)) = self.iter.next() {
                                    self.cur = Some((k.clone(), v));
                                    seed.deserialize(k.into_deserializer()).map(Some)
                                } else {
                                    Ok(None)
                                }
                            }

                            fn next_value_seed<VV>(
                                &mut self,
                                seed: VV,
                            ) -> Result<VV::Value, Self::Error>
                            where
                                VV: DeserializeSeed<'de>,
                            {
                                let v = self.cur.take().unwrap().1;
                                seed.deserialize(v.into_deserializer())
                            }
                        }
                        // Use BTreeMap for stable ordering in tests
                        let bt: std::collections::BTreeMap<_, _> = obj.into_iter().collect();
                        visitor.visit_map(JsonMapAccess { iter: bt.into_iter(), cur: None })
                    }
                }
            }
        }
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match (self.v, self.t) {
            (Value::Null, _) => visitor.visit_none(),
            (v, Type::Nullable(inner)) => {
                TypedDeserializer { v, t: inner }.deserialize_any(visitor)
            }
            (v, Type::LowCardinality(inner)) => {
                TypedDeserializer { v, t: inner }.deserialize_option(visitor)
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_enum<V>(
        self,
        _name: &str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

/// Row-level deserializer: emits a map of column name -> typed value
pub struct RowDeserializer<'a> {
    pub cols: &'a [(String, Type)],
    pub row: &'a [Value],
}

impl<'de> Deserializer<'de> for RowDeserializer<'_> {
    type Error = serde_json::Error;

    // Delegate primitive type methods to deserialize_any
    forward_to_deserialize_any! {
        deserialize_bool,
        deserialize_i8, deserialize_i16, deserialize_i32, deserialize_i64,
        deserialize_u8, deserialize_u16, deserialize_u32, deserialize_u64,
        deserialize_f32, deserialize_f64,
        deserialize_char, deserialize_str, deserialize_string,
        deserialize_bytes, deserialize_byte_buf,
        deserialize_option, deserialize_seq,
        deserialize_identifier
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let cap = core::cmp::min(self.cols.len(), self.row.len());
        struct RowAccess<'a> {
            cols: &'a [(String, Type)],
            row: &'a [Value],
            idx: usize,
            cap: usize,
        }
        impl<'de> MapAccess<'de> for RowAccess<'_> {
            type Error = serde_json::Error;

            fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
            where
                K: DeserializeSeed<'de>,
            {
                if self.idx >= self.cap {
                    return Ok(None);
                }
                let key: &str = &self.cols[self.idx].0;
                seed.deserialize(key.into_deserializer()).map(Some)
            }

            fn next_value_seed<VV>(&mut self, seed: VV) -> Result<VV::Value, Self::Error>
            where
                VV: DeserializeSeed<'de>,
            {
                let i = self.idx;
                self.idx += 1;
                let ty = &self.cols[i].1;
                let v = &self.row[i];
                let de = TypedDeserializer { v, t: ty };
                seed.deserialize(de)
            }
        }
        visitor.visit_map(RowAccess { cols: self.cols, row: self.row, idx: 0, cap })
    }

    // Delegate other forms
    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::native::values::MultiPolygon;

    #[test]
    fn transcode_named_tuple_via_deserializer() {
        let cols = vec![(
            "my_tuple".into(),
            Type::TupleNamed(vec![("a".into(), Type::String), ("b".into(), Type::Int64)]),
        )];
        let row = vec![Value::Tuple(vec![Value::string("x"), Value::Int64(7)])];

        let de = RowDeserializer { cols: &cols, row: &row };
        let mut out = Vec::new();
        {
            let mut ser = serde_json::Serializer::new(&mut out);
            // We want a single row object
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!({"my_tuple": {"a":"x","b":7}}));
    }

    #[test]
    fn typed_deserializer_transcodes_primitives() {
        let ty = Type::Int32;
        let v = Value::Int32(42);
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        assert_eq!(serde_json::from_slice::<i32>(&out).unwrap(), 42);
    }

    #[test]
    fn typed_deserializer_transcodes_decimal_as_string() {
        let ty = Type::Decimal64(2);
        let v = Value::Decimal64(2, 1234);
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!("12.34"));
    }

    #[test]
    fn typed_deserializer_transcodes_map_with_stringified_keys() {
        let ty = Type::Map(Box::new(Type::Int64), Box::new(Type::String));
        let v = Value::Map(
            vec![Value::Int64(7), Value::Int64(42)],
            vec![Value::string("x"), Value::string("y")],
        );
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!({"7": "x", "42": "y"}));
    }

    #[test]
    fn typed_deserializer_transcodes_float_nan_to_null() {
        let ty = Type::Float64;
        let v = Value::Float64(f64::NAN);
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, serde_json::Value::Null);
    }

    #[test]
    fn typed_deserializer_transcodes_point() {
        let ty = Type::Point;
        let v = Value::Point(Point([1.0, -2.5]));
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!([1.0, -2.5]));
    }

    #[test]
    fn typed_deserializer_transcodes_ring() {
        let ty = Type::Ring;
        let v = Value::Ring(Ring(vec![Point([0.0, 0.0]), Point([1.0, 1.0])]));
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!([[0.0, 0.0], [1.0, 1.0]]));
    }

    #[test]
    fn typed_deserializer_transcodes_polygon() {
        let ty = Type::Polygon;
        let v = Value::Polygon(Polygon(vec![Ring(vec![
            Point([0.0, 0.0]),
            Point([1.0, 0.0]),
            Point([1.0, 1.0]),
            Point([0.0, 0.0]),
        ])]));
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!([[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]));
    }

    #[test]
    fn typed_deserializer_transcodes_multi_polygon() {
        let ty = Type::MultiPolygon;
        let v = Value::MultiPolygon(MultiPolygon(vec![
            Polygon(vec![Ring(vec![Point([0.0, 0.0]), Point([1.0, 0.0]), Point([1.0, 1.0])])]),
            Polygon(vec![Ring(vec![Point([2.0, 2.0]), Point([3.0, 2.0]), Point([3.0, 3.0])])]),
        ]));
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            got,
            json!([[[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]], [[[2.0, 2.0], [3.0, 2.0], [3.0, 3.0]]]])
        );
    }

    #[test]
    fn typed_deserializer_transcodes_object_bytes() {
        let ty = Type::Object;
        let bytes = serde_json::to_vec(&json!({"k": [1, 2, 3]})).unwrap();
        let v = Value::Object(bytes);
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!({"k": [1, 2, 3]}));
    }

    #[test]
    fn typed_deserializer_transcodes_variant_inner() {
        let ty = Type::Variant(vec![Type::Int64]);
        let v = Value::Variant(0, Box::new(Value::Int64(9)));
        let mut out = Vec::new();
        {
            let de = TypedDeserializer { v: &v, t: &ty };
            let mut ser = serde_json::Serializer::new(&mut out);
            serde_transcode::transcode(de, &mut ser).unwrap();
        }
        let got: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got, json!(9));
    }
}
