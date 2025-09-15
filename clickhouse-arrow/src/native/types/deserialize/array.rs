use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
use crate::{Result, Value};

/// Trait to allow reading `Item`s and packing them into a `Value::*`.
pub(crate) trait ArrayDeserializerGeneric {
    type Item;
    /// The type of the items, e.g. [Value]
    fn inner_type(type_: &Type) -> Result<&Type>;
    /// Mapping from items to the return Value, e.g. simply `Vec<Value> -> Value::Array(items)`.
    fn inner_value(items: Vec<Self::Item>) -> Value;
    /// Conversion between the [Value] read and the items, e.g. simply the identity.
    fn item_mapping(value: Value) -> Self::Item;
}

/// Simple case for reading into a [`Value::Array`].
pub(crate) struct ArrayDeserializer;
impl ArrayDeserializerGeneric for ArrayDeserializer {
    type Item = Value;

    fn inner_type(type_: &Type) -> Result<&Type> { type_.unwrap_array() }

    fn inner_value(items: Vec<Self::Item>) -> Value { Value::Array(items) }

    fn item_mapping(value: Value) -> Value { value }
}

impl<T: ArrayDeserializerGeneric + 'static> Deserializer for T {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        // Delegate to inner for non-sparse prefixes
        Self::inner_type(type_)?.deserialize_prefix_async(reader, state).await
    }

    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        if rows == 0 {
            return Ok(vec![]);
        }

        let mut offsets = Vec::with_capacity(rows);
        for _ in 0..rows {
            offsets.push(reader.read_u64_le().await?);
        }

        let mut items = Self::inner_type(type_)?
            .deserialize_column(reader, offsets[offsets.len() - 1] as usize, state)
            .await?
            .into_iter()
            .map(Self::item_mapping);

        let mut out = Vec::with_capacity(rows);
        let mut read_offset = 0u64;
        for offset in offsets {
            let len = offset - read_offset;
            read_offset = offset;
            #[expect(clippy::cast_possible_truncation)]
            out.push(Self::inner_value((&mut items).take(len as usize).collect()));
        }

        Ok(out)
    }

    
}

pub(crate) async fn read_with_path<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>> {
    if rows == 0 {
        return Ok(vec![]);
    }

    let mut offsets = Vec::with_capacity(rows);
    for _ in 0..rows {
        offsets.push(reader.read_u64_le().await?);
    }

    // Read flattened items
    path.push(0);
    let mut items = ArrayDeserializer::inner_type(type_)?
        .deserialize_column_with_path(reader, offsets[offsets.len() - 1] as usize, state, path)
        .await?
        .into_iter()
        .map(ArrayDeserializer::item_mapping);
    let _ = path.pop();

    let mut out = Vec::with_capacity(rows);
    let mut read_offset = 0u64;
    for offset in offsets {
        let len = offset - read_offset;
        read_offset = offset;
        #[expect(clippy::cast_possible_truncation)]
        out.push(ArrayDeserializer::inner_value((&mut items).take(len as usize).collect()));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use std::str::FromStr;
    use tokio::io::{AsyncRead, ReadBuf};

    // Minimal AsyncRead over Bytes
    struct BytesReader(bytes::Bytes);
    impl AsyncRead for BytesReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let to_copy = std::cmp::min(buf.remaining(), self.0.len());
            let chunk = self.0.split_to(to_copy);
            buf.put_slice(&chunk);
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn put_var_uint(buf: &mut BytesMut, mut value: u64) {
        let mut tmp = [0u8; 9];
        let mut pos = 0;
        while pos < 9 {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value > 0 { byte |= 0x80; }
            tmp[pos] = byte;
            pos += 1;
            if value == 0 { break; }
        }
        buf.extend_from_slice(&tmp[..pos]);
    }

    #[tokio::test]
    async fn array_of_tuple_with_sparse_element() {
        // Type: Array(Tuple(UInt64, UInt64))
        // 3 rows with offsets [2,2,3] -> total 3 items
        // Tuple e0 is SPARSE, e1 is DEFAULT
        let inner_tuple = Type::Tuple(vec![Type::UInt64, Type::UInt64]);
        let ty = Type::Array(Box::new(inner_tuple));

        let mut bytes = BytesMut::new();
        // Offsets per row
        bytes.extend_from_slice(&(2u64).to_le_bytes());
        bytes.extend_from_slice(&(2u64).to_le_bytes());
        bytes.extend_from_slice(&(3u64).to_le_bytes());

        // Now inner tuple data for 3 items:
        // Element 0 (UInt64, SPARSE over 3 items): indices at 0 and 2
        // Groups: 0 (first), 1 (gap to next), end flag 0 (no trailing)
        put_var_uint(&mut bytes, 0);
        put_var_uint(&mut bytes, 1);
        put_var_uint(&mut bytes, (1u64 << 62) | 0);
        // values: two u64 (for 2 indices)
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&9u64.to_le_bytes());

        // Element 1 (UInt64, DEFAULT): 3 values
        bytes.extend_from_slice(&100u64.to_le_bytes());
        bytes.extend_from_slice(&200u64.to_le_bytes());
        bytes.extend_from_slice(&300u64.to_le_bytes());

        let mut reader = BytesReader(bytes.freeze());
        let mut state = DeserializerState::default();
        use crate::native::test_helpers::mk_kind_plan;
        // Plan kinds: [] for Array (ignored), [0] for inner tuple, then elements
        state.kind_plan = Some(mk_kind_plan(&[
            (vec![], 0),       // array node
            (vec![0], 0),      // tuple node
            (vec![0, 0], 1),   // tuple e0 sparse
            (vec![0, 1], 0),   // tuple e1 default
        ]));

        let out = ty
            .deserialize_column(&mut reader, 3, &mut state)
            .await
            .expect("array(tuple) read");

        assert_eq!(out.len(), 3);
        // row0 has 2 items: (7,100), (0,200)
        if let Value::Array(items) = &out[0] {
            assert_eq!(items.len(), 2);
            if let Value::Tuple(v) = &items[0] {
                assert!(matches!(v[0], Value::UInt64(7)));
                assert!(matches!(v[1], Value::UInt64(100)));
            } else { panic!("expected tuple"); }
            if let Value::Tuple(v) = &items[1] {
                assert!(matches!(v[0], Value::UInt64(0)));
                assert!(matches!(v[1], Value::UInt64(200)));
            } else { panic!("expected tuple"); }
        } else { panic!("expected array"); }

        // row2 has 1 item: (9,300)
        if let Value::Array(items) = &out[2] {
            assert_eq!(items.len(), 1);
            if let Value::Tuple(v) = &items[0] {
                assert!(matches!(v[0], Value::UInt64(9)));
                assert!(matches!(v[1], Value::UInt64(300)));
            } else { panic!("expected tuple"); }
        } else { panic!("expected array"); }
    }

    #[tokio::test]
    async fn array_of_nested_tuple_sparse() {
        // Type: Array(Tuple(UUID, Tuple(UInt64)))
        // Plan kinds (synthetic):
        // []            = DEFAULT (array)
        // [0]           = DEFAULT (outer tuple)
        // [0,0]         = SPARSE  (UUID)
        // [0,1]         = DEFAULT (inner tuple)
        // [0,1,0]       = SPARSE  (UInt64)
        let inner = Type::Tuple(vec![Type::UInt64]);
        let tuple = Type::Tuple(vec![Type::Uuid, inner]);
        let ty = Type::Array(Box::new(tuple));

        // Offsets for 2 rows: [1,3] -> total 3 items
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&(1u64).to_le_bytes());
        bytes.extend_from_slice(&(3u64).to_le_bytes());

        // Now 3 tuple items flattened
        // UUID (sparse) across 3: present at item 0 and 2
        put_var_uint(&mut bytes, 0); // first value immediately
        put_var_uint(&mut bytes, 1); // gap of 1 till next
        put_var_uint(&mut bytes, (1u64 << 62) | 0); // end, no trailing defaults
        let u0a: u64 = 0x01010101_02020202; let u0b: u64 = 0x03030303_04040404;
        let u2a: u64 = 0xa0a0a0a0_b0b0b0b0; let u2b: u64 = 0xc0c0c0c0_d0d0d0d0;
        bytes.extend_from_slice(&u0a.to_le_bytes()); bytes.extend_from_slice(&u0b.to_le_bytes());
        bytes.extend_from_slice(&u2a.to_le_bytes()); bytes.extend_from_slice(&u2b.to_le_bytes());

        // Inner UInt64 (sparse) across 3: present at item 1 only
        put_var_uint(&mut bytes, 1); // one default before first value
        put_var_uint(&mut bytes, (1u64 << 62) | 1); // end with one trailing default
        bytes.extend_from_slice(&777u64.to_le_bytes());

        let mut reader = BytesReader(bytes.freeze());
        let mut state = DeserializerState::default();
        use crate::native::test_helpers::mk_kind_plan;
        state.kind_plan = Some(mk_kind_plan(&[
            (vec![], 0),        // array
            (vec![0], 0),       // outer tuple
            (vec![0, 0], 1),    // uuid sparse
            (vec![0, 1], 0),    // inner tuple
            (vec![0, 1, 0], 1), // inner uint64 sparse
        ]));

        let out = ty.deserialize_column(&mut reader, 2, &mut state).await.expect("array nested tuple");
        assert_eq!(out.len(), 2);

        // Row 0 has 1 item: (uuid0, (default))
        if let Value::Array(items) = &out[0] {
            assert_eq!(items.len(), 1);
            if let Value::Tuple(t) = &items[0] {
                if let Value::Uuid(u) = t[0] {
                    assert_eq!(u.as_u128(), ((u128::from(u0a) << 64) | u128::from(u0b)));
                } else { panic!("uuid0"); }
                if let Value::Tuple(it) = &t[1] {
                    assert!(matches!(it[0], Value::UInt64(0)));
                } else { panic!("inner"); }
            } else { panic!("tuple"); }
        } else { panic!("array"); }

        // Row 1 has 2 items: (default uuid, (777)), (uuid2, (default))
        if let Value::Array(items) = &out[1] {
            assert_eq!(items.len(), 2);
            if let Value::Tuple(t) = &items[0] {
                if let Value::Uuid(u) = t[0] { assert_eq!(u.as_u128(), 0); } else { panic!("uuid"); }
                if let Value::Tuple(it) = &t[1] { assert!(matches!(it[0], Value::UInt64(777))); } else { panic!("inner"); }
            } else { panic!("tuple"); }
            if let Value::Tuple(t) = &items[1] {
                if let Value::Uuid(u) = t[0] {
                    assert_eq!(u.as_u128(), ((u128::from(u2a) << 64) | u128::from(u2b)));
                } else { panic!("uuid2"); }
                if let Value::Tuple(it) = &t[1] { assert!(matches!(it[0], Value::UInt64(0))); } else { panic!("inner"); }
            } else { panic!("tuple"); }
        } else { panic!("array"); }
    }

    #[tokio::test]
    async fn nested_basic_deserialize() {
        // Nested(id UInt64, val UInt64) -> Array(Tuple(UInt64, UInt64))
        let ty = Type::from_str("Nested(id UInt64, val UInt64)").unwrap();

        // 2 rows, offsets [1,3] => total 3 items
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&(1u64).to_le_bytes());
        bytes.extend_from_slice(&(3u64).to_le_bytes());

        // Tuple field 0 (id): 3 values
        for v in [10u64, 20u64, 30u64] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // Tuple field 1 (val): 3 values
        for v in [100u64, 200u64, 300u64] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        let mut reader = BytesReader(bytes.freeze());
        let mut state = DeserializerState::default();

        let out = ty
            .deserialize_column(&mut reader, 2, &mut state)
            .await
            .expect("nested deserialize");

        assert_eq!(out.len(), 2);
        // Row 0: one item (10,100)
        if let Value::Array(items) = &out[0] {
            assert_eq!(items.len(), 1);
            if let Value::Tuple(t) = &items[0] {
                assert!(matches!(t[0], Value::UInt64(10)));
                assert!(matches!(t[1], Value::UInt64(100)));
            } else { panic!("expected tuple"); }
        } else { panic!("expected array"); }

        // Row 1: two items (20,200), (30,300)
        if let Value::Array(items) = &out[1] {
            assert_eq!(items.len(), 2);
            if let Value::Tuple(t) = &items[0] {
                assert!(matches!(t[0], Value::UInt64(20)));
                assert!(matches!(t[1], Value::UInt64(200)));
            } else { panic!("expected tuple"); }
            if let Value::Tuple(t) = &items[1] {
                assert!(matches!(t[0], Value::UInt64(30)));
                assert!(matches!(t[1], Value::UInt64(300)));
            } else { panic!("expected tuple"); }
        } else { panic!("expected array"); }
    }
}
