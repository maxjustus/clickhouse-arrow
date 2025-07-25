// TODO: Implement Variant serialization
//
// The Variant serialization should:
// 1. Sort variant types alphabetically to determine discriminator mapping
// 2. Write discriminators as a byte array (one byte per row)
// 3. Group values by discriminator
// 4. Serialize data for each discriminator in ascending discriminator order
// 5. Handle NULL values with discriminator 0xFF
// 6. Support nested/recursive variants
//
// Wire format:
// - [u8; rows] discriminators
// - For each unique discriminator in ascending order:
//   - Serialize all values with that discriminator using the corresponding type
//
// Example for Variant(String, UInt64) with values ["hello", 42, "world"]:
// - Discriminators: [0, 1, 0] (String=0, UInt64=1)
// - String data: "hello", "world" (2 rows with discriminator 0)
// - UInt64 data: 42 (1 row with discriminator 1)

use crate::io::ClickHouseWrite;
use crate::native::types::serialize::SerializerState;
use crate::native::types::Type;
use crate::native::values::Value;
use crate::Result;

pub(crate) struct VariantSerializer;

impl VariantSerializer {
    pub(crate) async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        _writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Variant doesn't have a prefix
        Ok(())
    }
    
    pub(crate) async fn write<W: ClickHouseWrite>(
        _type_: &Type,
        _values: Vec<Value>,
        _writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        todo!("Variant serialization not yet implemented")
    }
    
    pub(crate) fn write_sync<W: crate::io::ClickHouseBytesWrite>(
        _type_: &Type,
        _values: Vec<Value>,
        _writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        todo!("Variant sync serialization not yet implemented")
    }
}