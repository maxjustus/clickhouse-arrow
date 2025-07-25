# Variant Type TCP Dump Analysis

## Overview

This document analyzes the TCP dumps of ClickHouse queries involving Variant types and compares them with the protocol documentation.

## Test Queries

1. **First Query**: `SELECT if(number % 2 = 0, 'yes', number) as v FROM system.numbers LIMIT 3`
   - Returns `Variant(String, UInt64)` type
   - Values: 'yes', 1, 'yes'

2. **Second Query**: `SELECT if(number % 2 = 0, 'yes', toString(number)) as v FROM system.numbers LIMIT 3`
   - Returns plain `String` type (not Variant)
   - Values: 'yes', '1', 'yes'

3. **Third Query**: `SELECT if(number % 2 = 0, 'yes', [number, number + 1]) as v FROM system.numbers LIMIT 3`
   - Returns `Variant(Array(UInt64), String)` type
   - Values: 'yes', [1, 2], 'yes'

4. **Fourth Query**: `SELECT multiIf(number % 2 = 0, 'yes', number % 3 = 0, Now(), [number, number + 1]) as v FROM system.numbers LIMIT 3`
   - Returns `Variant(Array(UInt64), DateTime, String)` type
   - Values: 'yes', [1, 2], 'yes'

## Key Findings

### 1. Type Declaration

In the first query's response, we can see the Variant type declaration:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 0, 1, 118, 23, 86, 97, 114, 105, 97, 110, 116, 40, 83, 116, 114, 105, 110, 103, 44, 32, 85, 73, 110, 116, 54, 52, 41, 0]
```

Decoded:
- `118` (0x76) = 'v' (column name)
- `23` = length of type string
- `86, 97, 114, 105, 97, 110, 116, 40, 83, 116, 114, 105, 110, 103, 44, 32, 85, 73, 110, 116, 54, 52, 41` = "Variant(String, UInt64)"

### 2. Data Block Structure

The data block for the Variant type shows:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 3, 1, 118, 23, 86, 97, 114, 105, 97, 110, 116, 40, 83, 116, 114, 105, 110, 103, 44, 32, 85, 73, 110, 116, 54, 52, 41, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 3, 121, 101, 115, 3, 121, 101, 115, 1, 0, 0, 0, 0, 0, 0, 0]
```

Breaking down the data section after the type declaration:
- `0, 0, 0, 0, 0, 0, 0, 0` - Block info (8 bytes)
- `0, 0, 1, 0` - Discriminators: [0, 1, 0] indicating String, UInt64, String
- `3, 121, 101, 115, 3, 121, 101, 115` - String data: "yes", "yes" (with length prefixes)
- `1, 0, 0, 0, 0, 0, 0, 0` - UInt64 value: 1

### 3. Protocol Documentation vs Reality

The protocol documentation in `03-data-serialization.md` describes Variant serialization as:
```
// Stream 1: Discriminators with mode selection
writeVarUInt(serialization_mode, out);  // 0=BASIC, 1=COMPACT

// For PLAIN mode (mixed discriminators)
for (size_t i = 0; i < rows; ++i) {
    writeBinary(discriminators[i], out);  // UInt8 discriminator per value
}

// Stream 2+: Variant element streams using Native format
```

However, the actual TCP dump shows a simpler structure:
1. The discriminators are sent as a simple array without a mode prefix
2. The data for each variant type is serialized inline rather than in separate streams
3. The format appears more compact than described in the documentation

### 4. Discriminator Values

- `0` = First variant type (String)
- `1` = Second variant type (UInt64)
- `255` (0xFF) = NULL value (as documented)

### 5. Comparison with Non-Variant Query

The second query returns a plain String type:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 0, 1, 118, 6, 83, 116, 114, 105, 110, 103, 0]
```
- `118` = 'v' (column name)
- `6` = length of type string
- `83, 116, 114, 105, 110, 103` = "String"

The data is then serialized as a regular String column with offsets and character data.

## Implementation Notes

Based on this analysis:

1. **Variant Deserialization**: The actual wire format appears simpler than documented:
   - No serialization mode prefix
   - Discriminators sent as a simple byte array
   - Data for each variant type follows immediately after discriminators

2. **Current Implementation Issues**: The existing Variant deserializer implementation may be expecting the more complex multi-stream format described in the documentation, when the actual format is simpler.

3. **Recommended Fix**: The Variant deserializer should:
   - Read discriminators as a simple byte array (one byte per row)
   - Deserialize data for each variant type sequentially
   - Handle the inline format rather than expecting separate streams

### 6. Variant with Complex Types (Array)

The third query shows a Variant containing Array(UInt64) and String:

**Type Declaration**:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 0, 1, 118, 30, 86, 97, 114, 105, 97, 110, 116, 40, 65, 114, 114, 97, 121, 40, 85, 73, 110, 116, 54, 52, 41, 44, 32, 83, 116, 114, 105, 110, 103, 41, 0]
```
- `118` = 'v' (column name)
- `30` = length of type string
- `86, 97, 114, 105, 97, 110, 116, 40, 65, 114, 114, 97, 121, 40, 85, 73, 110, 116, 54, 52, 41, 44, 32, 83, 116, 114, 105, 110, 103, 41` = "Variant(Array(UInt64), String)"

**Data Block Structure**:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 3, 1, 118, 30, ..., 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 121, 101, 115, 3, 121, 101, 115]
```

Breaking down the data section:
- `0, 0, 0, 0, 0, 0, 0, 0` - Block info
- `1, 0, 1` - Discriminators: [String, Array, String]
- Array data:
  - `2, 0, 0, 0, 0, 0, 0, 0` - Array offset (2 elements)
  - `1, 0, 0, 0, 0, 0, 0, 0` - First element: 1
  - `2, 0, 0, 0, 0, 0, 0, 0` - Second element: 2
- String data:
  - `3, 121, 101, 115` - "yes" (length 3)
  - `3, 121, 101, 115` - "yes" (length 3)

**Important Observations**:
1. The discriminator order is different from the type declaration order
2. Array(UInt64) = discriminator 0
3. String = discriminator 1
4. The array data uses native format with 64-bit offsets
5. Data is still serialized inline, not in separate streams

## Conclusion

The TCP dumps reveal that the actual Variant wire format differs from the protocol documentation. The implementation should be adjusted to match the simpler format observed in the wire protocol rather than the more complex multi-stream format described in the documentation.

### 7. Variant with Three Types

The fourth query shows a Variant with three types:

**Type Declaration**:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 0, 1, 118, 40, 86, 97, 114, 105, 97, 110, 116, 40, 65, 114, 114, 97, 121, 40, 85, 73, 110, 116, 54, 52, 41, 44, 32, 68, 97, 116, 101, 84, 105, 109, 101, 44, 32, 83, 116, 114, 105, 110, 103, 41, 0]
```
- `118` = 'v' (column name)
- `40` = length of type string
- "Variant(Array(UInt64), DateTime, String)"

**Data Block Structure**:
```
bytes: [1, 0, 1, 0, 2, 255, 255, 255, 255, 0, 1, 3, 1, 118, 40, ..., 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 121, 101, 115, 3, 121, 101, 115]
```

Breaking down the data section:
- `0, 0, 0, 0, 0, 0, 0, 0` - Block info
- `2, 0, 2` - Discriminators: [String, Array, String]
- Array data:
  - `2, 0, 0, 0, 0, 0, 0, 0` - Array offset (2 elements)
  - `1, 0, 0, 0, 0, 0, 0, 0` - First element: 1
  - `2, 0, 0, 0, 0, 0, 0, 0` - Second element: 2
- String data:
  - `3, 121, 101, 115` - "yes" (length 3)
  - `3, 121, 101, 115` - "yes" (length 3)

**Important**: Even though the DateTime type is declared, it's never used in the data (no discriminator value 1), demonstrating that:
- Discriminator assignments are based on the full type list
- Array(UInt64) = 0, DateTime = 1, String = 2 (alphabetically sorted)

**Key Implementation Points**:
1. Discriminators are sent as a simple byte array
2. Data is serialized inline in the order it appears in the data
3. Arrays within variants use the native array format (64-bit offsets)
4. **The discriminator values are assigned based on alphabetical sorting of the variant type names**, not the declaration order:
   - `Variant(String, UInt64)`: String=0, UInt64=1
   - `Variant(Array(UInt64), String)`: Array(UInt64)=0, String=1
   - `Variant(String, UInt8)`: String=0, UInt8=1
   - `Variant(Array(UInt64), DateTime, String)`: Array(UInt64)=0, DateTime=1, String=2
   - This is consistent across all tested queries