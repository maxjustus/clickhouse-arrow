//! Streaming helpers toward a zero-copy pipeline.
//!
//! This initial scaffolding provides lightweight row wrappers that can serialize
//! using schema information without materializing intermediate JSON trees.
//! A true zero-copy `Deserializer` over raw bytes will be added in subsequent work.

use serde::ser::{Serialize, SerializeMap, Serializer};

use crate::native::types::Type;
use crate::native::values::serde_impls::Typed;
use crate::native::values::Value;

/// Placeholder for a future zero-copy lazy value.
/// For now, this simply references an existing `Value` with its `Type`.
pub struct LazyValue<'a> {
    pub v: &'a Value,
    pub t: &'a Type,
}

impl<'a> LazyValue<'a> {
    #[inline]
    pub fn get(&self) -> &'a Value { self.v }
}

/// Lightweight row wrapper for streaming serialization.
pub struct RawStreamingRow<'a> {
    pub schema: &'a [(String, Type)],
    pub values: Vec<LazyValue<'a>>, // parallel to schema
}

impl Serialize for RawStreamingRow<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let cap = core::cmp::min(self.schema.len(), self.values.len());
        let mut map = serializer.serialize_map(Some(cap))?;
        for i in 0..cap {
            let (ref name, ref ty) = self.schema[i];
            let lv = &self.values[i];
            map.serialize_entry(name, &Typed { v: lv.get(), t: ty })?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_streaming_row_serialize() {
        let schema = vec![
            ("t".into(), Type::TupleNamed(vec![("a".into(), Type::String), ("b".into(), Type::Int64)])),
            ("n".into(), Type::Nullable(Box::new(Type::Int32))),
        ];
        let row_vals = vec![
            Value::Tuple(vec![Value::string("z"), Value::Int64(1)]),
            Value::Null,
        ];
        let values = vec![
            LazyValue { v: &row_vals[0], t: &schema[0].1 },
            LazyValue { v: &row_vals[1], t: &schema[1].1 },
        ];
        let row = RawStreamingRow { schema: &schema, values };
        let got = serde_json::to_value(row).unwrap();
        assert_eq!(got, json!({"t": {"a": "z", "b": 1}, "n": null}));
    }
}
