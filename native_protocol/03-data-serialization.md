# ClickHouse Native TCP Protocol - Data Serialization Formats

This document provides byte-level specifications for the Native Block format and all data type serialization formats used in ClickHouse Native TCP protocol packets.

## Native TCP Protocol Format Overview

The Native TCP protocol format is ClickHouse's optimized binary format for TCP streaming between ClickHouse instances. Key characteristics:
- **Columnar block-oriented** structure for maximum efficiency
- **Direct memory layout** compatibility (`position_independent_encoding = false`)
- **Multi-stream architecture** for optimal compression per column
- **Type-aware serialization** with zero-copy deserialization  
- **Native format optimizations** enabled (`native_format = true`)
- **Version compatibility** across different ClickHouse releases

## Native Format Settings

The Native TCP protocol uses specific serialization settings that differ from Binary/RowBinary formats:

```cpp
// Native format serialization settings
// Source: src/Formats/NativeReader.cpp:98-100, src/Formats/NativeWriter.cpp:84-87
SerializeBinaryBulkSettings settings;
settings.position_independent_encoding = false;  // Use direct memory offsets
settings.native_format = true;                   // Enable Native optimizations
settings.low_cardinality_max_dictionary_size = 0; // No global dictionaries (Writer only)

// Additional Native-specific format settings (src/Formats/FormatSettings.h:512-519)
FormatSettings format_settings;
format_settings.native.allow_types_conversion = true;           // Default
format_settings.native.encode_types_in_binary_format = false;   // Default
format_settings.native.decode_types_in_binary_format = false;   // Default
format_settings.native.write_json_as_string = false;            // Default
format_settings.native.use_flattened_dynamic_and_json_serialization = false; // Default
```

**Key Differences from Binary Format:**
- **Position Independence**: `false` - uses direct memory offsets for efficiency
- **Native Optimizations**: `true` - enables ClickHouse-specific optimizations  
- **Dictionary Handling**: No global dictionaries, each block is self-contained

---

## 1. Native Block Structure

### 1.1 Overall Block Layout

```
[Complete Block Format]
1. BlockInfo (optional, based on client revision)
2. Column Count (VarUInt)
3. Row Count (VarUInt) 
4. For each column:
   - Column Name (String)
   - Column Type (String or binary-encoded)
   - Custom Serialization Info (optional)
   - Column Data (type-specific format)
```

### 1.2 BlockInfo Structure

BlockInfo contains block-level metadata using field-value encoding:

```cpp
// BlockInfo binary format
// Source: src/Core/BlockInfo.cpp:30-40
writeVarUInt(1, out); writeBool(is_overflows, out);    // Field 1: overflow flag
writeVarUInt(2, out); writeIntBinary(bucket_num, out); // Field 2: bucket number
writeVarUInt(0, out);                                  // Terminator
```

**Example BlockInfo Bytes:**
```
0x01 0x00                // Field 1: is_overflows = false
0x02 0xFF 0xFF 0xFF 0xFF // Field 2: bucket_num = -1 (no bucket)
0x00                     // Terminator
```

### 1.3 Column Header Format

Each column begins with metadata:

```cpp
// Source: src/Formats/NativeWriter.cpp:120-135
writeStringBinary(column_name, out);           // Column name
writeStringBinary(column_type_name, out);      // Type name or binary type
if (client_revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION) {
    ISerialization::writeSerializationInfo(out);  // Custom serialization
}
```

**Example Column Header:**
```
0x02 "id"     // Column name: "id" 
0x06 "UInt32" // Column type: "UInt32"
0x00          // No custom serialization (if supported)
```

---

## 2. Primitive Type Serialization

### 2.1 Integer Types

All integers use **little-endian** encoding:

| Type | Size | Format | Example Value | Example Bytes |
|------|------|--------|---------------|---------------|
| UInt8 | 1 byte | Direct | 255 | `0xFF` |
| UInt16 | 2 bytes | Little-endian | 1000 | `0xE8 0x03` |
| UInt32 | 4 bytes | Little-endian | 305419896 | `0x78 0x56 0x34 0x12` |
| UInt64 | 8 bytes | Little-endian | 1311768467463790320 | `0xF0 0xDE 0xBC 0x9A 0x78 0x56 0x34 0x12` |
| Int8 | 1 byte | Two's complement | -1 | `0xFF` |
| Int16 | 2 bytes | Two's complement LE | -1000 | `0x18 0xFC` |
| Int32 | 4 bytes | Two's complement LE | -305419896 | `0x88 0xA9 0xCB 0xED` |
| Int64 | 8 bytes | Two's complement LE | -1311768467463790320 | `0x10 0x21 0x43 0x65 0x87 0xA9 0xCB 0xED` |

### 2.2 Floating Point Types

IEEE 754 format, little-endian:

| Type | Size | Format | Example Value | Example Bytes |
|------|------|--------|---------------|---------------|
| Float32 | 4 bytes | IEEE 754 LE | 3.14159 | `0xD8 0x0F 0x49 0x40` |
| Float64 | 8 bytes | IEEE 754 LE | 3.141592653589793 | `0x18 0x2D 0x44 0x54 0xFB 0x21 0x09 0x40` |

### 2.3 Boolean Type

```cpp
// Bool serialization
writeBinary(static_cast<UInt8>(value ? 1 : 0), out);
```

**Format**: Single byte (0x00 = false, 0x01 = true)

---

## 3. String Type Serialization

### 3.1 String Type

**Per-value format:**
```
[VarUInt size][UTF-8 data]
```

**Column bulk format:**
```cpp
// String column serialization
// Source: src/DataTypes/Serializations/SerializationString.cpp:44-45
ColumnString::serialize() {
    // 1. Serialize offsets array (cumulative sizes)
    writeVarUInt(offsets.size(), out);
    for (size_t offset : offsets) {
        writeVarUInt(offset, out);
    }
    
    // 2. Serialize concatenated character data
    out.write(chars.data(), chars.size());
}
```

**Example String Column ("foo", "hello", ""):**
```
// Offsets array (cumulative)
0x03 // 3 strings
0x03 // Offset after "foo" (3 chars)
0x08 // Offset after "hello" (3+5 chars)  
0x08 // Offset after "" (3+5+0 chars)

// Character data
0x66 0x6F 0x6F           // "foo"
0x68 0x65 0x6C 0x6C 0x6F // "hello"
                         // "" (no bytes)
```

### 3.2 FixedString(N) Type

**Format**: Fixed N bytes per value, zero-padded if necessary

```cpp
// FixedString(10) serialization
// Source: src/DataTypes/Serializations/SerializationFixedString.cpp:50+
for (const auto & value : values) {
    out.write(value.data(), N);  // Always exactly N bytes
}
```

**Example FixedString(5) Column ("foo", "hello"):**
```
0x66 0x6F 0x6F 0x00 0x00 // "foo\0\0"
0x68 0x65 0x6C 0x6C 0x6F // "hello"
```

### 3.3 LowCardinality(String) Type

**Native format restrictions**: No global dictionaries allowed (`native_format = true`)

```cpp
// LowCardinality serialization (Native format)
// Source: src/DataTypes/Serializations/SerializationLowCardinality.cpp:437-530
void SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams() {
    // Native format enforces restriction on global dictionaries
    // Line 157-163: if (settings.native_format && need_global_dictionary)
    //     throw Exception("LowCardinality indexes serialization type for Native format 
    //                      cannot use global dictionary");
    
    // 1. Serialize dictionary keys
    dict_serialization->serializeBinaryBulkWithMultipleStreams(
        *keys_column, 0, keys_column->size(), settings, dictionary_state);
    
    // 2. Serialize indexes (UInt8/16/32/64 based on dictionary size)
    index_serialization->serializeBinaryBulkWithMultipleStreams(
        indexes_column, 0, limit, settings, indexes_state);
}
```

**Example LowCardinality(String) ("foo", "bar", "foo", "bar"):**
```
// Local dictionary (per-block)
0x02       // Dictionary size: 2
0x03 "foo" // Entry 0: "foo"  
0x03 "bar" // Entry 1: "bar"

// Indexes (UInt8 since dict_size <= 256)
0x04 // Index array size: 4
0x00 // "foo" (index 0)
0x01 // "bar" (index 1) 
0x00 // "foo" (index 0)
0x01 // "bar" (index 1)
```

**Native vs Binary Format Difference:**
- **Native**: No global dictionaries, each block is self-contained
- **Binary**: Can use shared global dictionaries across blocks for better compression

---

## 4. Date and Time Type Serialization

### 4.1 Date Type

**Format**: UInt16 (days since 1900-01-01)

```cpp
// Date serialization (days since 1900-01-01)
// Source: src/DataTypes/Serializations/SerializationDate.cpp:25+
UInt16 days = date_value - Date::DATE_LUT_MIN_YEAR * 365;  // Simplified
writeIntBinary(days, out);
```

**Example Date Values:**
- `1900-01-01` → `0x0000` 
- `2000-01-01` → `0x63 0x8E` (36563 days)
- `2024-01-01` → `0x2F 0xB7` (45231 days)

### 4.2 Date32 Type  

**Format**: Int32 (days since 1900-01-01, extended range)

```cpp
// Date32 serialization - supports wider date range
Int32 days = calculateDaysSince1900(date_value);
writeIntBinary(days, out);
```

### 4.3 DateTime Type

**Format**: UInt32 (Unix timestamp seconds)

```cpp
// DateTime serialization
// Source: src/DataTypes/Serializations/SerializationDateTime.cpp:30+
UInt32 timestamp = time_point_to_unix_timestamp(datetime_value);
writeIntBinary(timestamp, out);
```

**Example DateTime Values:**
- `1970-01-01 00:00:00` → `0x00 0x00 0x00 0x00`
- `2024-01-01 00:00:00` → `0x80 0x96 0x98 0x65` (1704067200)

### 4.4 DateTime64 Type

**Format**: Int64 (ticks since Unix epoch with configurable precision)

```cpp
// DateTime64 serialization
Int64 ticks = datetime_to_ticks(value, scale);  // scale: 3=ms, 6=μs, 9=ns
writeIntBinary(ticks, out);
```

**Example DateTime64 Values (scale=3, milliseconds):**
- `2024-01-01 00:00:00.123` → `0x7B 0x14 0x8E 0x7C 0x8E 0x01 0x00 0x00` (1704067200123)

---

## 5. UUID Type Serialization

**Format**: 16 bytes representing UUID as two UInt64 values

```cpp
// UUID serialization (little-endian)
// Source: src/DataTypes/Serializations/SerializationUUID.cpp:40+
writeUUIDText() {
    writeIntBinary(uuid.low, out);   // Lower 64 bits
    writeIntBinary(uuid.high, out);  // Upper 64 bits
}
```

**Example UUID `550e8400-e29b-41d4-a716-446655440000`:**
```
// Parsed as: 0x550e8400e29b41d4 (high), 0xa716446655440000 (low)  
0x00 0x00 0x44 0x55 0x66 0x44 0x16 0xA7  // Low part (little-endian)
0xD4 0x41 0x9B 0xE2 0x00 0x84 0x0E 0x55  // High part (little-endian)
```

---

## 6. Decimal Type Serialization

### 6.1 Decimal32/64/128

**Format**: Fixed-size signed integers with implicit scale

```cpp
// Decimal serialization
template<typename T>
writeDecimal(const Decimal<T> & value) {
    writeIntBinary(value.value, out);  // Raw integer representation
    // Scale is stored in type definition, not in data
}
```

**Example Decimal(10,2) Values:**
- `123.45` → stored as `12345` (Int32) → `0x39 0x30 0x00 0x00`
- `-67.89` → stored as `-6789` (Int32) → `0x7B 0xE5 0xFF 0xFF`

### 6.2 Decimal256

**Format**: 32-byte signed integer (little-endian)

```cpp
// Decimal256 serialization  
writeDecimal256(const Decimal256 & value) {
    // Write as 4 x UInt64 little-endian values
    for (int i = 0; i < 4; ++i) {
        writeIntBinary(value.items[i], out);
    }
}
```

---

## 7. Array Type Serialization

### 7.1 Array(T) Structure

**Native format uses direct memory offsets** (`position_independent_encoding = false`):

```cpp
// Array serialization (Native format - two streams)
// Source: src/DataTypes/Serializations/SerializationArray.cpp:301-352
void SerializationArray::serializeBinaryBulkWithMultipleStreams() {
    // Stream 1: ArraySizes - Direct UInt64 offsets (Native format)
    if (settings.position_independent_encoding)
        serializeArraySizesPositionIndependent(column, *stream, offset, limit);
    else
        SerializationNumber<ColumnArray::Offset>().serializeBinaryBulk(
            *column_array.getOffsetsPtr(), *stream, offset, limit);
    
    // Stream 2: ArrayElements - Flattened element data
    nested->serializeBinaryBulkWithMultipleStreams(
        nested_column, offset, limit, settings, state);
}
```

**Example Array(UInt32) Column ([[1,2], [3,4,5], []]):**
```
// Offsets stream (Native format - direct UInt64 offsets)
0x02 0x00 0x00 0x00 0x00 0x00 0x00 0x00    // Offset after [1,2] (2 elements)  
0x05 0x00 0x00 0x00 0x00 0x00 0x00 0x00    // Offset after [3,4,5] (2+3=5 elements)
0x05 0x00 0x00 0x00 0x00 0x00 0x00 0x00    // Offset after [] (5+0=5 elements)

// Elements stream (UInt32 values)
0x01 0x00 0x00 0x00    // Element 1
0x02 0x00 0x00 0x00    // Element 2
0x03 0x00 0x00 0x00    // Element 3  
0x04 0x00 0x00 0x00    // Element 4
0x05 0x00 0x00 0x00    // Element 5
```

**Native vs Binary Format Difference:**
- **Native**: Uses direct UInt64 offsets as stored in memory
- **Binary**: Would use VarInt sizes calculated from offsets for portability

### 7.2 Nested Arrays

**Nested arrays share offset streams** for memory efficiency:

Array(Array(UInt32)) uses:
1. **Outer offsets** - positions of inner arrays
2. **Inner offsets** - positions of elements within inner arrays  
3. **Element data** - flattened element values

---

## 8. Tuple Type Serialization

### 8.1 Tuple(T1, T2, ...) Structure

**Native format**: Separate streams for each tuple element (same as Binary format)

```cpp
// Tuple serialization (Native format)
// Source: src/DataTypes/Serializations/SerializationTuple.cpp:735-765
void SerializationTuple::serializeBinaryBulkWithMultipleStreams() {
    auto * tuple_state = checkAndGetState<SerializeBinaryBulkStateTuple>(state);

    for (size_t i = 0; i < elems.size(); ++i) {
        // Extract column for element i
        const auto & element_col = extractElementColumn(column, i);
        // Serialize each element using its own serialization and state
        elems[i]->serializeBinaryBulkWithMultipleStreams(
            element_col, offset, limit, settings, tuple_state->states[i]);
    }
}
```

**Example Tuple(UInt32, String) Column ((1,"foo"), (2,"bar")):**
```
// Element 0 stream (UInt32) - Native bulk serialization
0x01 0x00 0x00 0x00    // First tuple element 0: 1
0x02 0x00 0x00 0x00    // Second tuple element 0: 2

// Element 1 stream (String) - Native bulk serialization
// Uses direct memory offsets for String column
0x02                    // 2 strings
0x03                    // Offset after "foo"
0x06                    // Offset after "bar"
0x66 0x6F 0x6F         // "foo"
0x62 0x61 0x72         // "bar"
```

**Native vs Binary Format:**
- **Structure**: Identical multi-stream layout
- **Element Serialization**: Each element uses Native format optimizations

### 8.2 Named Tuples

**Same binary format** as unnamed tuples - names stored only in type definition.

---

## 9. Map Type Serialization

### 9.1 Map(K,V) Structure

**Native format implementation**: Map(K,V) = Array(Tuple(K,V)) with Native format optimizations

```cpp
// Map serialization (Native format - delegates to nested Array(Tuple(K,V)))
// Source: src/DataTypes/Serializations/SerializationMap.cpp:477-485
void SerializationMap::serializeBinaryBulkWithMultipleStreams() {
    // Map is implemented as Array(Tuple(keys, values))
    // Direct delegation to nested array serialization
    nested->serializeBinaryBulkWithMultipleStreams(
        extractNestedColumn(column), offset, limit, settings, state);
}
```

**Example Map(String,UInt32) {{"key1":1}, {"key2":2}}:**
```
// Outer array offsets (Native format - direct UInt64 offsets)
0x01 0x00 0x00 0x00 0x00 0x00 0x00 0x00    // Offset after first pair
0x02 0x00 0x00 0x00 0x00 0x00 0x00 0x00    // Offset after second pair

// Tuple element 0 (keys - String) - Native format
0x02                    // 2 strings
0x04                    // Offset after "key1"
0x08                    // Offset after "key2"  
0x6B 0x65 0x79 0x31    // "key1"
0x6B 0x65 0x79 0x32    // "key2"

// Tuple element 1 (values - UInt32) - Native bulk serialization
0x01 0x00 0x00 0x00    // Value 1
0x02 0x00 0x00 0x00    // Value 2
```

**Native vs Binary Format:**
- **Array Offsets**: Native uses direct UInt64 offsets instead of VarInt sizes
- **Tuple Elements**: Each element uses Native format optimizations
- **Performance**: Better bulk serialization performance in Native format

---

## 10. Nullable Type Serialization

### 10.1 Nullable(T) Structure

**Native format two-stream approach** (same as Binary format):

```cpp
// Nullable serialization (Native format)
// Source: src/DataTypes/Serializations/SerializationNullable.cpp:94-113
void SerializationNullable::serializeBinaryBulkWithMultipleStreams() {
    const ColumnNullable & col = assert_cast<const ColumnNullable &>(column);
    
    /// First serialize null map.
    settings.path.push_back(Substream::NullMap);
    if (auto * stream = settings.getter(settings.path))
        SerializationNumber<UInt8>().serializeBinaryBulk(
            col.getNullMapColumn(), *stream, offset, limit);

    /// Then serialize contents of arrays.
    settings.path.back() = Substream::NullableElements;
    nested->serializeBinaryBulkWithMultipleStreams(
        col.getNestedColumn(), offset, limit, settings, state);
}
```

**Example Nullable(UInt32) Column (1, NULL, 3):**
```
// Null bitmap stream (Native format - bulk UInt8 serialization)
0x00 // Position 0: not null
0x01 // Position 1: null
0x00 // Position 2: not null

// Data stream (only non-null values, Native format)
0x01 0x00 0x00 0x00 // Value at position 0: 1
0x03 0x00 0x00 0x00 // Value at position 2: 3
```

**Native vs Binary Format:**
- **Format**: Identical for Nullable types
- **Performance**: Native format uses bulk serialization for better efficiency

---

## 11. Enum Type Serialization

### 11.1 Enum8/Enum16 Structure

**Format**: Integer values with string mappings in type definition

```cpp
// Enum serialization
serializeEnum8() {
    for (const auto & value : enum_values) {
        writeIntBinary(static_cast<UInt8>(value), out);
    }
}
```

**Example Enum8('red'=1, 'green'=2, 'blue'=3) Column (red, blue, green):**
```
0x01 // "red" (value 1)
0x03 // "blue" (value 3)  
0x02 // "green" (value 2)
```

---

## 12. IP Address Type Serialization

### 12.1 IPv4 Type

**Format**: UInt32 (network byte order stored as little-endian)

```cpp
// IPv4 serialization
writeIPv4(const IPv4 & addr) {
    writeIntBinary(addr.toUnderType(), out);  // UInt32 little-endian
}
```

**Example IPv4 192.168.1.1:**
```
0x01 0x01 0xA8 0xC0 // 192.168.1.1 as UInt32 little-endian
```

### 12.2 IPv6 Type

**Format**: 16 bytes (stored as FixedString(16))

```cpp
// IPv6 serialization
writeIPv6(const IPv6 & addr) {
    out.write(addr.toUnderType().items, 16);  // Raw 16 bytes
}
```

---

## 13. Compression and Advanced Features

### 13.1 Multi-Stream Compression

Each logical stream can be **compressed independently**:

```cpp
// Stream compression
if (compression_enabled) {
    CompressedWriteBuffer compressed_out(out, compression_method);
    column.serialize(compressed_out);
    compressed_out.next();  // Finalize compressed block
}
```

### 13.2 Custom Serialization

Advanced types support **custom serialization metadata**:

```cpp
// Custom serialization info
writeCustomSerializationInfo() {
    writeVarUInt(serialization_kind, out);  // REGULAR=0, SPARSE=1, etc.
    if (serialization_kind == SPARSE) {
        writeVarUInt(default_value_index, out);
        // Additional sparse-specific metadata
    }
}
```

---

## Implementation Notes

### Performance Optimizations
- **Vectorized operations** for primitive types
- **SIMD instructions** for null bitmap processing  
- **Memory-mapped I/O** for large blocks
- **Zero-copy deserialization** where possible

### Memory Management
- **Streaming serialization** for large datasets
- **Incremental parsing** to limit memory usage
- **Buffer reuse** to reduce allocations

### Error Handling  
- **Size validation** prevents buffer overflows
- **Type compatibility** checked during deserialization
- **Partial cleanup** on parse errors

### Version Compatibility
- **Feature flags** control optional serialization features
- **Graceful degradation** for unsupported features  
- **Forward compatibility** through reserved fields

---

## 14. Variant Type Serialization

### 14.1 Variant(T1, T2, ...) Structure

**Native format**: Multi-stream with discriminator-based type selection and Native optimizations

```cpp
// Variant serialization (Native format)
// Source: src/DataTypes/Serializations/SerializationVariant.cpp:300+
serializeVariant() {
    // Stream 1: Discriminators with mode selection
    writeVarUInt(serialization_mode, out);  // 0=BASIC, 1=COMPACT
    
    if (mode == COMPACT) {
        // Granule-based compression
        for (each granule) {
            writeVarUInt(granule_size, out);
            writeVarUInt(granule_format, out);  // 0=PLAIN, 1=COMPACT
            if (granule_format == COMPACT) {
                writeIntBinary(discriminator, out);  // Single discriminator
            } else {
                // Write all discriminators in granule
                for (auto disc : granule_discriminators) {
                    writeIntBinary(disc, out);
                }
            }
        }
    } else {
        // BASIC mode: write all discriminators sequentially
        for (auto disc : all_discriminators) {
            writeIntBinary(disc, out);
        }
    }
    
    // Stream 2+: Variant element streams using Native format
    for (size_t i = 0; i < variant_types.size(); ++i) {
        variant_serializations[i]->serializeBinaryBulkWithMultipleStreams(
            variant_columns[i], offset, limit, settings, state);
    }
}
```

**Example Variant(String, UInt32, Array(UInt16)) Column:**

Data: `'hello', 42, [1,2,3], NULL, 'world'`

```
// Global type ordering (sorted by name):
// Array(UInt16) = discriminator 0
// String = discriminator 1  
// UInt32 = discriminator 2
// NULL = discriminator 255

// Discriminators stream (COMPACT mode)
0x01                     // COMPACT mode
0x05                     // Granule size: 5 values
0x00                     // PLAIN format (mixed discriminators)
0x01 0x02 0x00 0xFF 0x01 // [String, UInt32, Array, NULL, String]

// String variant stream (Native format)
0x02                     // 2 strings
0x05                     // Offset after "hello"
0x0A                     // Offset after "world"  
0x68 0x65 0x6C 0x6C 0x6F // "hello"
0x77 0x6F 0x72 0x6C 0x64 // "world"

// UInt32 variant stream (Native bulk serialization)
0x2A 0x00 0x00 0x00      // Value: 42

// Array(UInt16) variant stream (Native format - direct offsets)
0x03 0x00 0x00 0x00 0x00 0x00 0x00 0x00  // Direct UInt64 offset (3 elements)
0x01 0x00 0x02 0x00 0x03 0x00            // Elements: 1, 2, 3 (UInt16 LE)
```

### 14.2 Null Handling in Variants

**NULL_DISCRIMINATOR**: Special value `255` indicates NULL
- No data stored in variant columns for NULL values
- NULL bitmap not required since discriminator encodes null status

**Native vs Binary Format:**
- **Discriminator Stream**: Identical format
- **Variant Streams**: Each uses Native format optimizations (direct offsets, bulk serialization)

---

## 15. Dynamic Type Serialization  

### 15.1 Dynamic Structure

**Native format**: Adaptive variant with schema evolution and Native optimizations

```cpp
// Dynamic serialization (Native format - version 2)
// Source: src/DataTypes/Serializations/SerializationDynamic.cpp:400+
serializeDynamic() {
    // Stream 1: Structure metadata
    writeVarUInt(serialization_version, out);   // Version 2
    writeVarUInt(num_dynamic_types, out);       // Current variant count
    for (const auto & type_name : sorted_dynamic_types) {
        writeStringBinary(type_name, out);      // Dynamic type names
    }
    if (for_mergetree) {
        // Write statistics for MergeTree optimization
        writeStatistics(out);
    }
    
    // Stream 2: Variant column data using Native format
    variant_serialization->serializeBinaryBulkWithMultipleStreams(
        variant_column, offset, limit, settings, state);
}
```

**Example Dynamic Column with values:**
`'foo', 42, [1,2], 'bar', 99.5`

```
// Structure stream (version 2)
0x02                 // Serialization version
0x04                 // Number of dynamic types: 4
0x0C "Array(UInt64)" // Type name 0 (sorted order)
0x07 "Float64"       // Type name 1  
0x06 "String"        // Type name 2
0x06 "UInt64"        // Type name 3

// Data stream (ColumnVariant format using Native optimizations)
// ... discriminators and variant data as in Variant type, with Native format for each variant stream
```

### 15.2 SharedVariant Overflow Handling

When `max_dynamic_types` exceeded in Native format:

```cpp  
// SharedVariant format (Native): <encoded_type><serialized_value>
// Source: src/DataTypes/Serializations/SerializationDynamic.cpp:500+
serializeSharedVariant(const String & type_name, const IColumn & column, size_t row) {
    // Encode type as binary
    encodeDataType(type_name, out);
    // Serialize value using type's Native format serialization
    type_serialization->serializeBinary(column, row, out, settings);
}
```

**Example SharedVariant Value (DateTime type):**
```
0x08 "DateTime"             // Encoded type name
0x80 0x96 0x98 0x65        // DateTime value (2024-01-01 as UInt32)
```

**Native vs Binary Format:**
- **Structure Metadata**: Identical format
- **Variant Data**: Uses Native format optimizations for better performance
- **SharedVariant Values**: Same encoding but may benefit from Native format settings

---

## 16. JSON Type Serialization

### 16.1 JSON (Object) Structure

**Native format implementation**: JSON type uses `DataTypeObject` with hybrid storage and Native optimizations

```cpp
// JSON serialization (Native format - multi-stream columnar format)
// Source: src/DataTypes/Serializations/SerializationObject.cpp:200+
serializeJSON() {
    // Stream 1: Object structure metadata
    writeVarUInt(actual_dynamic_paths_count, out);
    writeArrayBinary(sorted_dynamic_paths, out);
    if (for_mergetree) {
        writeDynamicPathsStatistics(out);
        writeSharedDataPathsStatistics(out);
    }
    
    // Stream 2+: Typed path streams using Native format
    for (const auto & [path, column] : typed_paths) {
        typed_path_serializations[path]->serializeBinaryBulkWithMultipleStreams(
            column, offset, limit, settings, state);
    }
    
    // Stream N+: Dynamic path streams using Native Dynamic serialization
    for (const auto & [path, dynamic_column] : dynamic_paths) {
        dynamic_serialization->serializeBinaryBulkWithMultipleStreams(
            dynamic_column, offset, limit, settings, state);
    }
    
    // Final stream: Shared data using Native Array(Tuple(String, String)) format
    shared_data_serialization->serializeBinaryBulkWithMultipleStreams(
        shared_data_column, offset, limit, settings, state);
}
```

**Example JSON Column Storage:**

Document: `{"user": {"id": 123, "name": "John"}, "tags": ["A", "B"], "score": 95.5}`

**Storage breakdown:**
- **Typed Path**: `user.id` → UInt64 column  
- **Dynamic Path**: `score` → Dynamic column (could be Float64 or UInt64)
- **Shared Data**: `user.name`, `tags` → Array(Tuple(String, String))

```
// Structure stream
0x01         // 1 dynamic path
0x05 "score" // Dynamic path name

// Typed path stream (user.id as UInt64) - Native bulk serialization
0x7B 0x00 0x00 0x00 0x00 0x00 0x00 0x00 // Value: 123

// Dynamic path stream (score as Float64) - Native Dynamic serialization
[Dynamic column serialization for Float64 value 95.5 using Native format]

// Shared data stream (Array(Tuple(String, String))) - Native format
0x01 0x00 0x00 0x00 0x00 0x00 0x00 0x00  // Offset after tuple 1 (UInt64)
0x02 0x00 0x00 0x00 0x00 0x00 0x00 0x00  // Offset after tuple 2 (UInt64)

// Tuple 0 data (user.name)
0x09 "user.name" // Path
0x04 "John"      // Serialized value (String)

// Tuple 1 data (tags)  
0x04 "tags"       // Path
0x0E "['A', 'B']" // Serialized value (JSON array as string)
```

### 16.2 JSON Path Storage Strategies

**Three-tier storage architecture**:

1. **Typed Paths**: Frequently accessed, consistent type → Native column
2. **Dynamic Paths**: Frequently accessed, varying types → ColumnDynamic  
3. **Shared Data**: Infrequent/overflow paths → Serialized key-value pairs

**Path promotion logic**:
- Paths exceeding access thresholds get promoted to dynamic storage
- Dynamic paths with stable types get promoted to typed storage
- `max_dynamic_paths` parameter controls dynamic storage capacity

### 16.3 JSON Single Value Format

For individual JSON value serialization in Native format:

```cpp
// Single JSON value (Native format)
// Source: src/DataTypes/Serializations/SerializationObject.cpp:150+
serializeJSONValue() {
    // Serialize as complete JSON text using Native string serialization
    writeStringBinary(json_text, out, settings);
}
```

**Example**: `{"a": 1, "b": "test"}` → `0x12 {"a": 1, "b": "test"}`

**Native vs Binary Format:**
- **Structure Metadata**: Identical format
- **Typed Paths**: Use Native format optimizations for better performance
- **Dynamic Paths**: Use Native Dynamic serialization
- **Shared Data**: Uses Native Array/Tuple format with direct offsets
- **Overall Performance**: Significantly better for bulk JSON operations

---

## Implementation Notes for New Types

### Version Compatibility
- **Variant**: Requires client protocol version with variant support
- **Dynamic**: Built on Variant, requires same protocol version
- **JSON**: Uses Object serialization, may require schema negotiation

### Performance Characteristics  
- **Variant**: Optimal for known, limited type sets
- **Dynamic**: Good for evolving schemas with type adaptation
- **JSON**: Best for document-like data with path-based access

### Memory Management
- **Multi-stream architecture** enables efficient compression per stream
- **Lazy loading** of variant/dynamic columns based on query needs
- **Path-based indexing** for JSON enables selective column reading

---

## Native vs Binary Format Summary

This specification documents the **Native TCP protocol format** used for ClickHouse-to-ClickHouse communication. Key differences from Binary/RowBinary formats:

### Format Settings
| Setting | Native Format | Binary Format |
|---------|---------------|---------------|
| `position_independent_encoding` | `false` | `true` |
| `native_format` | `true` | `false` |
| `low_cardinality_max_dictionary_size` | `0` | configurable |

### Data Type Differences
| Type | Native Format | Binary Format |
|------|---------------|---------------|
| **Arrays** | Direct UInt64 offsets | VarInt sizes |
| **LowCardinality** | Per-block dictionaries only | Can use global dictionaries |
| **Complex Types** | Bulk serialization APIs | Standard serialization |
| **All Types** | Memory layout optimized | Portability optimized |

### Performance Characteristics
- **Native Format**: Optimized for ClickHouse-to-ClickHouse streaming
  - Direct memory layout compatibility
  - Bulk serialization operations
  - Better compression ratios with block-level compression
- **Binary Format**: Optimized for portability and external integration
  - Position-independent encoding
  - Easier to implement in external systems
  - Better for row-by-row processing

This specification enables bit-exact reproduction of ClickHouse's Native TCP protocol format, supporting full protocol compatibility and efficient data exchange between ClickHouse instances.
