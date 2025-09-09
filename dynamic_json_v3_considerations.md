# Dynamic/JSON v3 (flattened) Native format considerations

This document summarizes the ClickHouse v3 (flattened) native serialization for Dynamic and JSON(Object) columns, maps it to this codebase, and lists practical gotchas and refactor suggestions. It also proposes how to handle arrays and nested collection types to match the canonical implementation rather than falling back to String.

## What the upstream change introduces

- Dynamic (flattened, version = 3)
  - Structure stream: list of all data types present in the column (either names or binary-encoded types)
  - Data stream: indexes (UInt8/16/32/64 chosen by number of types), then column data for each type in the same order
  - Null is encoded as discriminator/index equal to types.len()
  - Only used in Native format when `output_format_native_use_flattened_dynamic_and_json_serialization` is enabled

- Object/JSON (flattened, version = 3)
  - ObjectStructure: list of all flattened dynamic/shared-data paths (typed paths are NOT included here)
  - ObjectData: first all typed paths (each with their own custom serialization), then for each dynamic or flattened shared-data path a Dynamic(v3) column
  - Also gated behind `output_format_native_use_flattened_dynamic_and_json_serialization` in Native format

- Types encoding knob
  - `output_format_native_encode_types_in_binary_format` / `input_format_native_decode_types_in_binary_format` allow type definitions to be encoded/decoded in binary rather than strings inside the structure streams

## Where this repo already aligns

- Dynamic
  - Already implements v3 (flattened): writes version=3, varuint type count, sorted type names, nested type prefixes, discriminators with sized width, then typed columns; read side mirrors this. Null uses `disc = total_types`.

- JSON/Object
  - Implementation sends: version=3, total paths (sorted), then for each path a Dynamic(v3) header and its data; read side consumes the same and reconstructs JSON string objects.
  - Conceptually “reuses Dynamic per path”.

## Implement arrays and nested collections (match canonical)

Goal: preserve arrays and nested collections as native types (Array, Map, Tuple, Variant, Nullable) in per-path Dynamic columns rather than collapsing them to String.

- Path traversal
  - Objects: keep recursing into members to produce leaf paths (e.g., `user.name`, `user.age`). Do not treat whole objects as leaf values for per-path columns.
  - Arrays: treat the entire array as a leaf value for the current path and convert JSON array elements recursively to native Values. Do not explode arrays by index during serialization (i.e., do not create `path[0]`, `path[1]` columns). This matches the upstream wire format which carries a single Dynamic column per path.
    - **Note**: This correctly matches the native *wire format*, but differs from the server's `SELECT` query engine which can extract paths by index (e.g., `json.a[1].b`). The goal is to match the wire format, not replicate the query engine's full JSONPath semantics.

- Value conversion (JSON → Value) rules
  - Null → Value::Null
  - Bool → Value::UInt8 (0/1)
  - Number → Int64/UInt64/Float64 based on JSON number range
  - String → Value::String
  - Array → Value::Array, with recursive conversion for elements
    - Mixed element types → element type becomes Variant(T1, T2, …) inferred during Dynamic analysis
    - Nulls inside arrays → element type wrapped as Nullable(T)
    - Nested arrays → Value::Array of arrays with recursive inference
  - Object (as leaf) → avoid when possible by further recursion; if encountered (e.g., array contains objects):
    - Option A (strict canonical leaning): convert each object element to Value::Map if keys are strings and values convert to scalars/arrays recursively
    - Option B (fallback): convert the object element to Value::String (JSON) only when it is not representable as Map/Tuple; prefer A to avoid String fallback

- Type inference
  - Use existing Value::guess_type to derive Type for each Value; it already handles heterogeneous arrays by emitting Variant(…)
  - Ensure the Dynamic per-path type registry is built from the Value set (not ad-hoc type-name heuristics)

- Serialization flow (per path)
  - Analyze: build type registry from the converted Values (arrays preserved). This yields stable type names and nested prefixes for complex inner types
  - Prefix: write Dynamic v3 prefix using those types and nested prefixes for complex types (e.g., Array(Decimal(…)), Array(Array(Int32)), Array(Variant(…)))
  - Data: write discriminators and then per-type columns according to the Dynamic v3 rules

- Deserialization flow (per path)
  - Read Dynamic v3 prefix and nested prefixes
  - Read discriminators and per-type columns
  - Reconstruct Value for each row: preserve Array/Map/Tuple/Variant structure
  - Rebuild output JSON string by merging all paths per row; path leaves that are Arrays/Maps are converted back to JSON arrays/objects

- Arrays-of-objects
  - For arrays where elements are objects:
    - Attempt to convert elements to consistent Map(String → Value) if keys are strings; this yields a legal ClickHouse Map type per element
    - If object shapes vary, the Dynamic analysis will induce Variant across multiple Array element types (e.g., Array(Variant(Map(String, T), Array(U), …)))
    - Only fall back to Value::String for element objects as a last resort when no safe native representation is possible

- Limits
  - Enforce reasonable recursion depth to prevent pathological inputs
  - Cap Variant alternatives to a practical number (matching or below server limits) and degrade to String only if absolutely necessary

## Gotchas and integration edges

1) Typed paths in JSON/Object
- In flattened JSON, typed paths (declared in the JSON type) are serialized first in ObjectData and do NOT appear in the flattened path list.
- If typed paths are present in the schema, the stream interleaves typed-path substreams ahead of the per-path Dynamic blocks. Readers that assume only dynamic-path Dynamic blocks will misalign.
- If you must support typed paths:
  - Read prefix: consume typed-path prefixes before dynamic-path prefixes.
  - Read data: read typed-path data before the dynamic-path per-path data.
  - Write: emit typed-path blocks before dynamic-path blocks.
- If you don’t support them yet, keep `typed_paths` empty and document it. This simplification is recommended, as full support also requires separating values into typed vs. dynamic columns on the write side and emitting typed-path substreams first.

2) Shared data flattening
- Upstream flattens Object “shared data” into synthetic paths in flattened mode; on read those appear just like dynamic paths. Your per-path Dynamic reader will handle them.
- On write (client → server), you send JSON strings; server may unflatten back into shared data if limits require it.

3) Binary-encoded types vs type names
- Flattened structure streams may use binary-encoded types. Current code reads/writes type names (strings) only.
- Either implement binary type encoding/decoding in the structure phases or error out cleanly if the binary setting is detected.

4) Index width and null encoding
- Index width must be chosen from u8/u16/u32/u64 by number of types; null encoded as `disc = total_types`. Current macros follow this.

5) Nested type prefixes
- Complex types require nested prefixes before their data. Dynamic v3 already does this; JSON per-path reuses Dynamic and is covered.

6) Path ordering
- Flattened paths are lexicographically sorted. Keep JSON paths strictly sorted from analysis through prefix/data phases (use BTreeMap + explicit sorting).

7) Offset/limit semantics in flattened Dynamic
- Upstream flattened Dynamic write only supports whole-column serialization. Do not splice partial segments inside a single Dynamic block.

8) Type inference differences
- Upstream can infer types like Date from strings in JSON typed paths. Current mapping is explicit and conservative; results (type sets/discriminators) may differ from server-produced data. Consider optional inference for well-known string-encoded primitives if stricter parity is needed.

9) Stability of type sets across blocks
- Structure and nested prefixes are per-block. If you split columns across multiple blocks with differing type sets, the receiver must tolerate that. Prefer stable type sets per logical column or single-block batches when possible.

10) Nested JSON/Dynamic recursion
- Keep guardrails to avoid JSON nested inside JSON. Continue reusing Dynamic for per-path serialization.

11) Performance/memory
- JSON analyze_values builds full per-path columns; large object variety can be memory-heavy. Consider chunking/streaming if needed.

## Refactor suggestions to reuse Dynamic more

- Delegate per-path JSON work to Dynamic:
  - Analyze: for each path’s values (after recursive JSON → Value conversion), call `DynamicSerializer::analyze_values` to build the type registry
  - Prefix: call `DynamicSerializer::write_prefix` for that path’s Dynamic column
  - Data: call `DynamicSerializer::write` for that path’s values
  - Deserialize: mirror via `DynamicDeserializer::{read_prefix, read}` for each path

- Typed paths (if/when supported):
  - Read prefix: consult Type::JSON typed_paths and consume those prefixes before dynamic-path prefixes
  - Read data: read typed-path data first, then per-path dynamic data
  - Reconstruct: merge typed-path results with dynamic-path results into a single JSON object per row

## Tests to add

- JSON with arrays
  - Arrays of scalars (homogeneous and mixed types) → ensure Array(T)/Array(Variant(…)) serialization
  - Nested arrays (e.g., Array(Array(Int32))) → nested prefixes and data
  - Arrays containing nulls → Nullable element types

- JSON with arrays of objects
  - Consistent object keys → representable as Array(Map(String, T))
  - Mixed object shapes → induce Array(Variant(Map(String, T1), Map(String, T2), …)) rather than String fallback

- JSON with typed_paths in schema
  - Ensure reader can consume typed-path substreams (or error clearly if unsupported)

- JSON with many distinct paths (shared-data flattening on server)
  - Roundtrip through server Native format to validate flattened shared-data paths are handled on read

- Dynamic index width boundaries
  - total_types around 255, 256, 65535, 65536 to verify index width and null encoding

- Complex nested types requiring prefixes
  - e.g., Array(Decimal(..)), DateTime64 with tz, nested Arrays/Tuples

- Binary type encoding
  - If implemented, roundtrip both string and binary type encodings

## Minimal change set (arrays-first)

- JSON serialization
  - Update recursive conversion from `serde_json::Value` to `Value` to preserve arrays (and nested arrays) instead of collapsing arrays to String
  - Leave objects as traversal-only (continue to descend to leaves for per-path columns); only treat objects as leaf values inside arrays where needed

- JSON deserialization
  - No change to wire format; ensure reconstructed JSON emits arrays/objects faithfully from preserved Value trees

- Dynamic delegation
  - Replace custom per-path type-name heuristics with `DynamicSerializer::analyze_values` on the converted Values

- Fallback policy
  - Only fallback to String for array elements that are objects that cannot be represented as Map/Tuple/Array/Variant without exploding complexity or violating constraints

## Notes

- Keep strict ordering of paths and types for reproducible structure streams
- Maintain version gating and fail early on unsupported versions/settings
- Avoid partial segment writes for flattened Dynamic within a block
- Enforce recursion depth and Variant alternatives caps to prevent pathological inputs

## Client-side SKIP and limits (flattened v3)

Flattened JSON v3 treats the ObjectStructure (flattened paths list) as authoritative for ingestion. The server does not apply SKIP/SKIP REGEXP itself at ingest time. The practical implications for the client are:

- Apply SKIP in the client:
  - SKIP exact: filter out non-typed paths whose full dotted path equals the value (e.g., `password`).
  - SKIP REGEXP: filter out non-typed paths that match the regex on the full dotted path (e.g., `secret.*`).
  - Typed paths always win over SKIP. If a path is declared typed in the schema, it is included regardless of SKIP rules.

- Do not enforce `max_dynamic_paths` client-side for v3:
  - Send all discovered (non-skipped) dynamic paths in the flattened header; the server decides which to materialize as dynamic subcolumns and which to move into shared data.

- Do not enforce `max_dynamic_types` client-side for v3:
  - Build the per-path Dynamic type registry from observed values and send all types; the server can accept or constrain types according to its configuration.

This approach keeps client behavior aligned with ClickHouse’s v3 design: the client prepares a faithful flattened view (minus SKIPped paths), and the server performs final selection and layout.
