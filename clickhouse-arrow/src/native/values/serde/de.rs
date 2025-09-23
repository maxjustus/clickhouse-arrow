use ::serde::Deserializer;
use ::serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use crate::native::types::Type;
use crate::native::values::Value;

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
        let t = self.t.strip_null();
        match (self.v, t) {
            (Value::Null, _) => visitor.visit_none(),
            (v, Type::Nullable(inner)) => {
                TypedDeserializer { v, t: inner }.deserialize_any(visitor)
            }

            (Value::Array(items), Type::Array(inner)) => {
                struct ArrAccess<'a> {
                    items: &'a [Value],
                    idx:   usize,
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
                    idx:    usize,
                    cap:    usize,
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
                    types:  &'a [Type],
                    idx:    usize,
                    cap:    usize,
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
                            cur:  Option<(String, serde_json::Value)>,
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
    pub row:  &'a [Value],
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
            row:  &'a [Value],
            idx:  usize,
            cap:  usize,
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
}
