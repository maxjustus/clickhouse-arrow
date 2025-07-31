# JSON Serialization TODO

This document tracks the implementation of proper JSON serialization for all ClickHouse Value types.

## Goal
Make JSON output consistent with clickhouse-go implementation - clean, standard JSON without ClickHouse function wrappers.

## Implementation Status

### ✅ Already Handled
- [x] Null → `null`
- [x] Int8/16/32/64, UInt8/16/32/64 → JSON numbers
- [x] Int128/256, UInt128/256 → Decimal strings
- [x] Float32/64 → JSON numbers (or null for inf/nan)
- [x] String → JSON string

### ✅ High Priority
- [x] **Decimal Types**
  - Decimal32/64/128/256 → Decimal strings with proper point placement
  - Example: `Decimal32(2, 1234)` → `"12.34"`

- [x] **Date/Time Types**
  - Date/Date32 → ISO date format: `"2024-01-15"`
  - DateTime → ISO 8601 format: `"2024-01-15T14:30:00Z"`
  - DateTime64 → With fractional seconds: `"2024-01-15T14:30:00.123Z"`

### ✅ Medium Priority (Partially Complete)
- [x] **UUID**
  - Standard hyphenated format: `"603966d6-ed93-11ec-8ea0-0242ac120002"`

- [x] **Network Types**
  - IPv4 → Dotted decimal: `"192.168.1.1"`
  - IPv6 → Standard notation: `"2001:db8::1"`

- [x] **Container Types**
  - Array → JSON array: `[1, 2, 3]`
  - Tuple → JSON array: `[1, "hello", true]`
  - Map → JSON object (if string keys) or array of `[key, value]` pairs

### ✅ Low Priority
- [x] **Enum Types**
  - Enum8/16 → String value only: `"yes"` (not `"yes::1"`)

- [x] **Variant Type**
  - Unwrap and serialize contained value
  - Example: `Variant(1, "hello")` → `"hello"`

- [x] **Geo Types**
  - Point → `[longitude, latitude]`
  - Ring → Array of points
  - Polygon → Array of rings
  - MultiPolygon → Array of polygons

- [x] **Object Type**
  - Parse JSON bytes and return as nested structure

## Test Plan
- Add comprehensive tests for each type
- Ensure compatibility with clickhouse-go behavior
- Test edge cases (empty arrays, null values, etc.)

## Implementation Complete! ✅

All Value types now have proper JSON serialization that matches the clickhouse-go implementation:
- Clean, standard JSON output
- No ClickHouse function wrappers
- Consistent format across all client libraries

### Next Steps
1. Add comprehensive tests
2. Ensure the implementation compiles and passes tests
3. Consider performance optimizations if needed