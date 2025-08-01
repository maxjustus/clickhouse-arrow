use arrow::array::*;
use tokio::io::AsyncWriteExt;

use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Type};

/// Serializes an Arrow array to `ClickHouse`'s native format for string or binary types.
///
/// Dispatches to specialized serialization functions based on the `Type` variant:
/// - `String`: Serializes variable-length strings with length prefixes using `write_string_values`.
/// - `Binary`: Serializes variable-length binary data using `write_binary_values`.
/// - `FixedSizedString(len)`: Serializes fixed-length strings with padding using
///   `write_fixed_string_values`.
/// - `FixedSizedBinary(len)`: Serializes fixed-length binary data with padding using
///   `write_fixed_binary_values`.
///
/// # Arguments
/// - `type_hint`: The `ClickHouse` `Type` indicating the target type (`String`, `Binary`, etc.).
/// - `values`: The Arrow array containing the data to serialize.
/// - `writer`: The async writer to serialize to (e.g., a TCP stream).
///
/// # Returns
/// A `Result` indicating success or a `Error` if serialization fails.
///
/// # Errors
/// - Returns `ArrowSerialize` if the `type_hint` is unsupported or the Arrow array type is
///   incompatible.
/// - Returns `Io` if writing to the writer fails.
pub(super) async fn serialize_async<W: ClickHouseWrite>(
    type_hint: &Type,
    writer: &mut W,
    values: &ArrayRef,
) -> Result<()> {
    match type_hint.strip_null() {
        Type::String | Type::Object => write_string_values(values, writer).await?,
        Type::Binary => write_binary_values(values, writer).await?,
        Type::FixedSizedString(len) => write_fixed_string_values(values, writer, *len).await?,
        Type::FixedSizedBinary(len) => write_fixed_binary_values(values, writer, *len).await?,
        _ => {
            return Err(Error::ArrowSerialize(format!("Unsupported data type: {type_hint:?}")));
        }
    }

    Ok(())
}

pub(super) fn serialize<W: ClickHouseBytesWrite>(
    type_hint: &Type,
    writer: &mut W,
    values: &ArrayRef,
) -> Result<()> {
    match type_hint.strip_null() {
        Type::String | Type::Object => put_string_values(values, writer)?,
        Type::Binary => put_binary_values(values, writer)?,
        Type::FixedSizedString(len) => put_fixed_string_values(values, writer, *len)?,
        Type::FixedSizedBinary(len) => put_fixed_binary_values(values, writer, *len)?,
        _ => {
            return Err(Error::ArrowSerialize(format!("Unsupported data type: {type_hint:?}")));
        }
    }

    Ok(())
}

/// Macro to generate serialization functions for variable-length string or binary types.
///
/// Generates functions that write data with length prefixes (for `String`) or raw bytes (for
/// `Binary`). Supports multiple Arrow array types via downcasting, handling nulls by writing empty
/// data.
macro_rules! write_variable_values {
    ($name:ident, varlen $write_fn:ident, $def:expr, [$(($at:ty => $coerce:expr)),* $(,)?]) => {
        /// Serializes an Arrow array to ClickHouse's native format for variable-length data.
        ///
        /// Writes each value using the specified write function (e.g., `write_string` for `String`,
        /// and `Binary`). Null values are written as empty data. Supports multiple Arrow
        /// array types via downcasting.
        ///
        /// # Arguments
        /// - `column`: The Arrow array containing the data.
        /// - `writer`: The async writer to serialize to.
        ///
        /// # Returns
        /// A `Result` indicating success or a `Error` if the array type is unsupported.
        async fn $name<W: ClickHouseWrite>(
            column: &::arrow::array::ArrayRef,
            writer: &mut W,
        ) -> Result<()> {
            $(
                if let Some(array) = column.as_any().downcast_ref::<$at>() {
                    for i in 0..array.len() {
                        let value = if array.is_null(i) {
                            $def
                        } else {
                            $coerce(array.value(i))
                        };
                        writer.$write_fn(value).await?;
                    }
                    return Ok(());
                }
            )*

            Err($crate::Error::ArrowSerialize(
                concat!("Expected one of: ", $(stringify!($at), " "),*).into()
            ))
        }
    };
}

macro_rules! put_variable_values {
    ($name:ident, varlen $write_fn:ident, $def:expr, [$(($at:ty => $coerce:expr)),* $(,)?]) => {
        /// Serializes an Arrow array to ClickHouse's native format for variable-length data.
        ///
        /// Writes each value using the specified write function (e.g., `write_string` for `String`,
        /// and `Binary`). Null values are written as empty data. Supports multiple Arrow
        /// array types via downcasting.
        ///
        /// # Arguments
        /// - `column`: The Arrow array containing the data.
        /// - `writer`: The async writer to serialize to.
        ///
        /// # Returns
        /// A `Result` indicating success or a `Error` if the array type is unsupported.
        fn $name<W: $crate::io::ClickHouseBytesWrite>(
            column: &::arrow::array::ArrayRef,
            writer: &mut W,
        ) -> Result<()> {
            $(
                if let Some(array) = column.as_any().downcast_ref::<$at>() {
                    for i in 0..array.len() {
                        let value = if array.is_null(i) {
                            $def
                        } else {
                            $coerce(array.value(i))
                        };
                        writer.$write_fn(value)?;
                    }
                    return Ok(());
                }
            )*

            Err($crate::Error::ArrowSerialize(
                concat!("Expected one of: ", $(stringify!($at), " "),*).into()
            ))
        }
    };
}

/// Macro to generate serialization functions for fixed-length string or binary types.
///
/// Generates functions that write data padded to a fixed length with zeros if necessary. Null
/// values are written as zeroed buffers of the expected length. Supports multiple Arrow array types
/// via downcasting.
macro_rules! write_fixed_values {
    // Fixed-size with dynamic length (e.g., FixedSizedString)
    ($name:ident, [$(($at:ty => $coerce:expr)),* $(,)?]) => {
        /// Serializes an Arrow array to ClickHouse's native format for fixed-length data.
        ///
        /// Writes each value padded to the specified length with zeros if shorter, or truncated if
        /// longer. Null values are written as zeroed buffers of the expected length. Supports multiple
        /// Arrow array types via downcasting.
        ///
        /// # Arguments
        /// - `column`: The Arrow array containing the data.
        /// - `writer`: The async writer to serialize to.
        /// - `len`: The fixed length expected by `ClickHouse`.
        ///
        /// # Returns
        /// A `Result` indicating success or a `Error` if the array type is unsupported.
        async fn $name<W: ClickHouseWrite>(
            column: &::arrow::array::ArrayRef,
            writer: &mut W,
            len: usize
        ) -> Result<()> {
            let expected_len = len;
            $(
                if let Some(array) = column.as_any().downcast_ref::<$at>() {
                    for i in 0..array.len() {
                        if array.is_null(i) {
                            writer.write_all(&vec![0u8; expected_len]).await?;
                            continue;
                        }

                        let value = $coerce(array.value(i));
                        if value.len() != expected_len {
                            let mut padded = vec![0u8; expected_len];
                            let copy_len = value.len().min(expected_len);
                            padded[..copy_len].copy_from_slice(&value[..copy_len]);
                            writer.write_all(&padded).await?;
                        } else {
                            writer.write_all(&value).await?;
                        };
                    }
                    return Ok(());
                }
            )*
            Err($crate::Error::ArrowSerialize(
                concat!("Expected one of: ", $(stringify!($at), " "),*).into()
            ))
        }
    };
}

macro_rules! put_fixed_values {
    // Fixed-size with dynamic length (e.g., FixedSizedString)
    ($name:ident, [$(($at:ty => $coerce:expr)),* $(,)?]) => {
        /// Serializes an Arrow array to ClickHouse's native format for fixed-length data.
        ///
        /// Writes each value padded to the specified length with zeros if shorter, or truncated if
        /// longer. Null values are written as zeroed buffers of the expected length. Supports multiple
        /// Arrow array types via downcasting.
        ///
        /// # Arguments
        /// - `column`: The Arrow array containing the data.
        /// - `writer`: The async writer to serialize to.
        /// - `len`: The fixed length expected by `ClickHouse`.
        ///
        /// # Returns
        /// A `Result` indicating success or a `Error` if the array type is unsupported.
        fn $name<W: $crate::io::ClickHouseBytesWrite>(
            column: &::arrow::array::ArrayRef,
            writer: &mut W,
            len: usize
        ) -> Result<()> {
            let expected_len = len;
            $(
                if let Some(array) = column.as_any().downcast_ref::<$at>() {
                    for i in 0..array.len() {
                        if array.is_null(i) {
                            writer.put_slice(&vec![0u8; expected_len]);
                            continue;
                        }

                        let value = $coerce(array.value(i));
                        if value.len() != expected_len {
                            let mut padded = vec![0u8; expected_len];
                            let copy_len = value.len().min(expected_len);
                            padded[..copy_len].copy_from_slice(&value[..copy_len]);
                            writer.put_slice(&padded);
                        } else {
                            writer.put_slice(&value);
                        };
                    }
                    return Ok(());
                }
            )*
            Err($crate::Error::ArrowSerialize(
                concat!("Expected one of: ", $(stringify!($at), " "),*).into()
            ))
        }
    };
}

write_variable_values!(write_string_values, varlen write_string, &[], [
    (StringArray => as_bytes),
    (BinaryArray => pass_through),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeStringArray => as_bytes),
    (LargeBinaryArray => pass_through)
]);
write_variable_values!(write_binary_values, varlen write_string, &[], [
    (BinaryArray => pass_through),
    (StringArray => as_bytes),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeBinaryArray => pass_through),
    (LargeStringArray => as_bytes)
]);

put_variable_values!(put_string_values, varlen put_string, &[], [
    (StringArray => as_bytes),
    (BinaryArray => pass_through),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeStringArray => as_bytes),
    (LargeBinaryArray => pass_through)
]);
put_variable_values!(put_binary_values, varlen put_string, &[], [
    (BinaryArray => pass_through),
    (StringArray => as_bytes),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeBinaryArray => pass_through),
    (LargeStringArray => as_bytes)
]);

write_fixed_values!(write_fixed_string_values, [
    (StringArray => as_bytes),
    (FixedSizeBinaryArray => pass_through),
    (BinaryArray => pass_through),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeStringArray => as_bytes),
    (LargeBinaryArray => pass_through)
]);
write_fixed_values!(write_fixed_binary_values, [
    (FixedSizeBinaryArray => pass_through),
    (BinaryArray => pass_through),
    (LargeBinaryArray => pass_through),
    (BinaryViewArray => pass_through),
    (StringArray => as_bytes),
    (StringViewArray => as_bytes),
    (LargeStringArray => as_bytes)
]);

put_fixed_values!(put_fixed_string_values, [
    (StringArray => as_bytes),
    (FixedSizeBinaryArray => pass_through),
    (BinaryArray => pass_through),
    (StringViewArray => as_bytes),
    (BinaryViewArray => pass_through),
    (LargeStringArray => as_bytes),
    (LargeBinaryArray => pass_through)
]);
put_fixed_values!(put_fixed_binary_values, [
    (FixedSizeBinaryArray => pass_through),
    (BinaryArray => pass_through),
    (LargeBinaryArray => pass_through),
    (BinaryViewArray => pass_through),
    (StringArray => as_bytes),
    (StringViewArray => as_bytes),
    (LargeStringArray => as_bytes)
]);

/// Coerces a byte slice to itself (no-op).
fn pass_through(v: &[u8]) -> &[u8] { v }

/// Coerces a string to its byte representation.
fn as_bytes(v: &str) -> &[u8] { v.as_bytes() }

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{BinaryArray, FixedSizeBinaryArray, Int32Array, StringArray};

    use super::*;

    type MockWriter = Vec<u8>;

    macro_rules! binary_test {
        ($name:ident, $type_hint:expr, $array:expr, $expected:expr) => {
            #[tokio::test]
            async fn $name() {
                let col = Arc::new($array) as ArrayRef;
                let expected = $expected;

                // Test async
                let mut async_writer = MockWriter::new();
                serialize_async(&$type_hint, &mut async_writer, &col).await.unwrap();
                assert_eq!(async_writer, expected);

                // Test sync
                let mut sync_writer = MockWriter::new();
                serialize(&$type_hint, &mut sync_writer, &col).unwrap();
                assert_eq!(sync_writer, expected);
            }
        };
    }

    macro_rules! binary_error_test {
        ($name:ident, $type_hint:expr, $array:expr, $error_check:expr) => {
            #[tokio::test]
            async fn $name() {
                let col = Arc::new($array) as ArrayRef;

                // Test async
                let mut async_writer = MockWriter::new();
                let async_result = serialize_async(&$type_hint, &mut async_writer, &col).await;
                assert!($error_check(&async_result));

                // Test sync
                let mut sync_writer = MockWriter::new();
                let sync_result = serialize(&$type_hint, &mut sync_writer, &col);
                assert!($error_check(&sync_result));
            }
        };
    }

    binary_test!(
        test_serialize_string,
        Type::String,
        StringArray::from(vec![Some("hello"), None, Some("world")]),
        vec![
            5, 104, 101, 108, 108, 111, // "hello" (var_uint 5 + bytes)
            0,   // "" (null, var_uint 0)
            5, 119, 111, 114, 108, 100, // "world" (var_uint 5 + bytes)
        ]
    );

    #[tokio::test]
    async fn test_serialize_string_empty_and_large() {
        let large_string = "x".repeat(128); // Test var_uint >127
        let column = Arc::new(StringArray::from(vec![Some(""), Some(&large_string), Some("abc")]))
            as ArrayRef;

        let mut expected = vec![0]; // "" (var_uint 0)
        expected.extend(vec![128, 1]); // var_uint 128 (128 = 128 + 1<<7)
        expected.extend(vec![120; 128]); // 128 'x' bytes
        expected.extend(vec![3, 97, 98, 99]); // "abc" (var_uint 3 + bytes)

        // Test async
        let mut async_writer = MockWriter::new();
        serialize_async(&Type::String, &mut async_writer, &column).await.unwrap();
        assert_eq!(async_writer, expected);

        // Test sync
        let mut sync_writer = MockWriter::new();
        serialize(&Type::String, &mut sync_writer, &column).unwrap();
        assert_eq!(sync_writer, expected);
    }

    binary_test!(
        test_serialize_string_unicode,
        Type::String,
        StringArray::from(vec![Some("こんにちは"), Some("")]),
        vec![
            15, // var_uint 15 (length of "こんにちは" in UTF-8)
            227, 129, 147, 227, 130, 147, 227, 129, 171, 227, 129, 161, 227, 129,
            175, // "こんにちは"
            0,   // "" (var_uint 0)
        ]
    );

    binary_test!(
        test_serialize_binary,
        Type::Binary,
        BinaryArray::from(vec![Some(b"abc".as_ref()), None, Some(b"def".as_ref())]),
        vec![
            3, 97, 98, 99, // "abc" (var_uint 3 + bytes)
            0,  // "" (null, var_uint 0)
            3, 100, 101, 102, // "def" (var_uint 3 + bytes)
        ]
    );

    #[tokio::test]
    async fn test_serialize_binary_empty_and_large() {
        let large_binary = vec![255u8; 300]; // Test large binary data
        let column = Arc::new(BinaryArray::from(vec![
            Some(b"".as_ref()),
            Some(large_binary.as_ref()),
            Some(b"xyz".as_ref()),
        ])) as ArrayRef;

        let mut expected = vec![0]; // "" (var_uint 0)
        expected.extend(vec![172, 2]); // var_uint 300 (300 = 44 + 2<<7)
        expected.extend(vec![255; 300]); // 300 0xFF bytes
        expected.extend(vec![3, 120, 121, 122]); // "xyz" (var_uint 3 + bytes)

        // Test async
        let mut async_writer = MockWriter::new();
        serialize_async(&Type::Binary, &mut async_writer, &column).await.unwrap();
        assert_eq!(async_writer, expected);

        // Test sync
        let mut sync_writer = MockWriter::new();
        serialize(&Type::Binary, &mut sync_writer, &column).unwrap();
        assert_eq!(sync_writer, expected);
    }

    binary_test!(
        test_serialize_fixed_string,
        Type::FixedSizedString(3),
        StringArray::from(vec![Some("ab"), None, Some("cd")]),
        vec![
            97, 98, 0, // "ab" + padding
            0, 0, 0, // null (all zeros)
            99, 100, 0, // "cd" + padding
        ]
    );

    binary_test!(
        test_serialize_fixed_string_short_and_null,
        Type::FixedSizedString(3),
        StringArray::from(vec![Some("a"), None, Some("")]),
        vec![
            97, 0, 0, // "a" + padding
            0, 0, 0, // null (all zeros)
            0, 0, 0, // empty string + padding
        ]
    );

    binary_test!(
        test_serialize_fixed_string_oversized,
        Type::FixedSizedString(3),
        StringArray::from(vec![Some("abcd")]),
        vec![97, 98, 99] // "abc" (truncated)
    );

    binary_test!(
        test_serialize_fixed_binary,
        Type::FixedSizedBinary(3),
        FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some(b"ab".as_ref()), None, Some(b"cd".as_ref())].into_iter(),
            2,
        )
        .unwrap(),
        vec![
            97, 98, 0, // "ab" + padding
            0, 0, 0, // null (all zeros)
            99, 100, 0, // "cd" + padding
        ]
    );

    binary_test!(
        test_serialize_fixed_binary_null,
        Type::FixedSizedBinary(3),
        FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some(b"ab".as_ref()), None].into_iter(),
            2,
        )
        .unwrap(),
        vec![
            97, 98, 0, // "ab" + padding
            0, 0, 0, // null (all zeros)
        ]
    );

    #[tokio::test]
    async fn test_serialize_fixed_binary_oversized() {
        let column = Arc::new(
            FixedSizeBinaryArray::try_from_iter(vec![b"abcd".as_ref()].into_iter()).unwrap(),
        ) as ArrayRef;

        // Test async
        let mut async_writer = MockWriter::new();
        let async_result =
            serialize_async(&Type::FixedSizedBinary(3), &mut async_writer, &column).await;
        assert!(async_result.is_ok(), "Expected truncated binary");

        // Test sync
        let mut sync_writer = MockWriter::new();
        let sync_result = serialize(&Type::FixedSizedBinary(3), &mut sync_writer, &column);
        assert!(sync_result.is_ok(), "Expected truncated binary");
    }

    binary_test!(
        test_serialize_empty_string,
        Type::String,
        StringArray::from(Vec::<String>::new()),
        Vec::<u8>::new()
    );

    binary_test!(
        test_serialize_empty_binary,
        Type::Binary,
        BinaryArray::from(Vec::<Option<&[u8]>>::new()),
        Vec::<u8>::new()
    );

    binary_test!(
        test_serialize_empty_fixed_string,
        Type::FixedSizedString(3),
        StringArray::from(Vec::<String>::new()),
        Vec::<u8>::new()
    );

    binary_test!(
        test_serialize_null_only_string,
        Type::String,
        StringArray::from(Vec::<Option<String>>::from([None, None])),
        vec![0, 0] // Two nulls
    );

    binary_error_test!(
        test_serialize_unsupported_type,
        Type::String,
        Int32Array::from(vec![1, 2, 3]),
        |result: &Result<()>| matches!(
            result,
            Err(Error::ArrowSerialize(msg))
            if msg.contains("Expected one of")
        )
    );

    binary_error_test!(
        test_serialize_invalid_array_type,
        Type::String,
        Int32Array::from(vec![1, 2, 3]),
        |result: &Result<()>| matches!(
            result,
            Err(Error::ArrowSerialize(msg))
            if msg.contains("Expected one of")
        )
    );
}
