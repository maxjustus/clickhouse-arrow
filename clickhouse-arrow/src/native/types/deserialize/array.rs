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
        state.cur_path.push(0);
        let res = Self::inner_type(type_)?.deserialize_prefix_async(reader, state).await;
        let _ = state.cur_path.pop();
        res
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

        // Track path: single child index 0 for arrays
        state.cur_path.push(0);
        let mut items = Self::inner_type(type_)?
            .deserialize_column(reader, offsets[offsets.len() - 1] as usize, state)
            .await?
            .into_iter()
            .map(Self::item_mapping);
        let _ = state.cur_path.pop();

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

    // sync array deserialization removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
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
}
