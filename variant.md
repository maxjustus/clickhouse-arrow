# ClickHouse Variant Type Binary Serialization in TCP Native Protocol

The ClickHouse Variant data type implements a discriminated union that enables storing heterogeneous values within a single column. For driver implementers, understanding its binary serialization format is crucial for correctly handling Variant types over the TCP Native wire protocol.

## Core Source Code Implementation

The Variant type implementation spans several key files in the ClickHouse repository:

**Primary implementation files:**
- `src/DataTypes/DataTypeVariant.cpp/h` - Main DataType implementation
- `src/Columns/ColumnVariant.cpp/h` - Column storage implementation  
- `src/DataTypes/Serializations/SerializationVariant.cpp/h` - Binary serialization logic

**Key classes and methods:**
- `DB::DataTypeVariant` - Data type class
- `DB::ColumnVariant` - Column storage
- `DB::SerializationVariant` - Handles binary serialization
- Methods: `serializeBinary()`, `deserializeBinary()`, `serializeBinaryBulkWithMultipleStreams()`

## Binary Format Structure

### Type Definition Encoding

The Variant type uses a specific binary encoding format when transmitted over the wire:

```
0x2A<var_uint_number_of_variants><variant_type_encoding_1>...<variant_type_encoding_N>
```

Where:
- **`0x2A`** is the type identifier for Variant in ClickHouse's binary encoding
- **`var_uint_number_of_variants`** specifies the number of variant types (maximum 255)
- Each **`variant_type_encoding_X`** follows standard ClickHouse data type binary encoding

For example, `Variant(Int64, String, Array(UInt64))` encodes as:
```
0x2A 03 0A 15 1E04
```
- `0x2A`: Variant type marker
- `03`: Three variant types
- `0A`: Int64 type encoding
- `15`: String type encoding
- `1E04`: Array(UInt64) type encoding

## Discriminator Encoding and Storage

The discriminator indicates which variant type is active for each row. ClickHouse implements two serialization modes:

### Basic Mode
All discriminators serialized as **UInt8 values** row by row:
```
[discriminator_1][discriminator_2]...[discriminator_N]
```

### Compact Mode (Default)
Controlled by `use_compact_variant_discriminators_serialization` setting. For granules where all discriminators are identical:

```
<number_of_rows_in_granule><granule_format><granule_data>
```

**Granule format types:**
- **`0x00` (Plain)**: Different discriminators in granule - all values serialized
- **`0x01` (Compact)**: Single discriminator for entire granule - stores only 3 values instead of 8,192

This optimization significantly reduces storage for sparse data patterns common in JSON scenarios.

**Discriminator value ranges:**
- **0-254**: Index into sorted list of variant type names
- **255**: Reserved for NULL values

## Multi-Stream Architecture

A Variant column transmits as multiple binary streams:

1. **Discriminator stream** (`column_name.variant_discr.bin`)
   - UInt8 values indicating active variant per row
   - Uses compact or basic serialization mode

2. **Type-specific data streams** (one per variant type)
   - Format: `column_name.TypeName.bin` (e.g., `C.Int64.bin`, `C.String.bin`)
   - **Dense storage**: Only actual values, no NULLs or defaults
   - Each stream uses standard serialization for its type

3. **In-memory offset mapping** (computed at runtime)
   - UInt64 values mapping discriminator positions to type-specific file positions
   - Not transmitted over wire, reconstructed from discriminator values

## Practical Binary Example

Consider a table with `Variant(Int64, String)` containing:
```
[42, "hello", NULL, 99, "world", NULL]
```

**Binary representation:**
- **Discriminator stream**: `[0, 1, 255, 0, 1, 255]`
- **Int64 stream**: `[42, 99]` (only actual Int64 values)
- **String stream**: `["hello", "world"]` (only actual String values)

**Offset reconstruction:**
- Row 0 (discriminator 0) → Int64 offset 0 → value 42
- Row 1 (discriminator 1) → String offset 0 → value "hello"
- Row 3 (discriminator 0) → Int64 offset 1 → value 99
- Row 4 (discriminator 1) → String offset 1 → value "world"

## Wire Protocol Transmission

Over the TCP Native protocol, Variant columns transmit as:

1. **Type declaration phase**: Variant type encoded with `0x2A` prefix and variant specifications
2. **Data transmission phase**: 
   - Discriminator stream (with format indicator for compact/plain mode)
   - Each variant type stream sequentially (dense format)
3. **No separate NULL mask**: Discriminator 255 handles NULL indication

**Little-endian encoding** applies to all fixed-size integers. Variable-length strings use `(varint_length, utf8_value)` format.

## Special Considerations and Edge Cases

### NULL Handling
Unlike other ClickHouse types, Variants don't require a separate NULL mask file. **Discriminator value 255** uniquely identifies NULL rows, which consume no space in variant data streams.

### Nested Variants
Variants can contain other Variants as types. Type order is normalized: `Variant(T1, T2)` equals `Variant(T2, T1)`. The discriminator values always map to the sorted type name list.

### Maximum Type Limit
The **UInt8 discriminator** limits each Variant to 255 different concrete types, with one value reserved for NULL. Driver implementations must validate this constraint.

### Compact Mode Detection
Drivers must detect granule format bytes (`0x00` or `0x01`) to correctly deserialize discriminator streams. Compact mode can reduce discriminator storage from 8,192 bytes to just 3 bytes per granule.

### Error Handling
Key validation points from `SerializationVariant::deserializeBinaryBulkWithMultipleStreams`:
- Array bounds checking on discriminator values
- Type validation during deserialization  
- Graceful handling of malformed binary data

## Key Differences from Other Types

Variant serialization differs fundamentally from standard ClickHouse types:

1. **Multiple storage streams** instead of single data streams
2. **Dense storage** without default/NULL value placeholders
3. **Runtime type resolution** through discriminator lookups
4. **Granule-level optimization** with compact discriminator mode
5. **No type coercion** - preserves original data types exactly

## Implementation Settings

Critical settings for driver implementations:
- `allow_experimental_variant_type` - Enable Variant usage
- `use_compact_variant_discriminators_serialization` - Compact mode (default: enabled)
- `allow_suspicious_variant_types` - Permit similar types in same Variant

## Conclusion

ClickHouse's Variant binary format optimizes for both storage efficiency and type safety through its discriminated union approach. Driver implementers must handle the multi-stream architecture, discriminator encoding modes, and dense storage format to correctly serialize and deserialize Variant types over the TCP Native protocol. The compact discriminator optimization and 255-type limit represent key implementation constraints that significantly impact wire protocol handling.
