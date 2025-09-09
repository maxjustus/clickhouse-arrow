Objective
- Support non-stringified JSON end-to-end: avoid unnecessary stringify→parse cycles on read/write, while keeping backward compatibility for clients that prefer strings.

Scope
- JSON (v3 flattened) native read/write paths.
- Keep serde optional; do not require a new enum variant unless gated behind `serde`.

Design Overview
- Deserialization (read): emit JSON bytes as `Value::Object(Vec<u8>)` rather than `Value::String`. This removes a stringify step and preserves a single parse when consumers request `serde_json::Value`.
- Serialization (write): accept structured JSON without re-parsing by allowing `Value::Object(Vec<u8>)` (bytes) in addition to the current `Value::String` (JSON text). Prefer `Object` when the source was already structured.
- Backward compatibility: keep accepting string inputs for inserts, and keep conversions via `FromSql` working for `serde_json::Value` and `String`.

Phases
1) Deserialization
   - Change `JsonDeserializer::build_json_objects` to push `Value::Object(Vec<u8>)` using `serde_json::to_vec` instead of `Value::String`.
   - Ensure `FromSql<serde_json::Value>` remains compatible (it already supports `Value::Object`).
   - Ensure `FromSql<String>` works as expected when reading `JSON` (convert JSON bytes to UTF-8 string as needed).

2) Serialization
   - Update `JsonSerializer::from_values` to accept both `Value::Object(Vec<u8>)` and `Value::String(Vec<u8>)` as input rows for JSON; parse from bytes when given `Object`, parse from text when given `String`.
   - Update `map_cell_to_value` for `Type::JSON`:
     - If the user provided a structured `serde_json::Value`, serialize once to bytes and return `Value::Object(bytes)` (instead of `Value::String`).
     - If the user provided a string of JSON, keep `Value::String` (back-compat and convenience).

3) Structured variant (required, gated by `serde`)
   - Introduce `Value::Json(serde_json::Value)` behind the `serde` feature for zero-parse handoff to serde consumers.
   - Deserializer emits `Value::Json` (no intermediate bytes) when `serde` is enabled.
   - Serializer consumes `Value::Json` directly (no parse).
   - `FromSql<serde_json::Value>` fast-paths `Value::Json` with a move.

Risks and Compatibility
- Returning `Value::Object` instead of `Value::String` from the deserializer changes the internal `Value` shape. Conversions to `serde_json::Value` and `String` must remain seamless via `FromSql` to avoid breaking users.
- Tests that asserted on `String` internals for raw JSON columns may need to read through `FromSql<String>` or use `toJSONString()` at the SQL layer.

Validation Plan
- Update or add tests that:
  - Read a JSON column and decode to `serde_json::Value` (no stringify involved).
  - Read a JSON column and decode to `String` (ensures `FromSql<String>` still works from `Value::Object`).
  - Insert via `serde_json::Value` and verify no redundant parse is performed by accepting `Value::Object` in the serializer.

Implementation Checklist
- [x] Change `build_json_objects` to emit `Value::Object` with `to_vec`.
- [x] Adjust serializer to accept `Value::Object` in addition to `Value::String`.
- [x] Change `map_cell_to_value` to prefer `Value::Object` when input is structured.
- [x] Ensure `FromSql<String>` path handles `Value::Object` (already supported).
- [x] Add `Value::Json(serde_json::Value)` (serde-gated), with PartialEq/Hash/to_json support.
- [x] Deserializer emits `Value::Json` when serde is enabled.
- [x] Serializer consumes `Value::Json` directly.
- [x] `FromSql<serde_json::Value>` fast-paths `Value::Json`.
- [x] Update tests and docs accordingly (added unit tests; existing integrations remain valid).

Progress Notes
- Deserializer emits `Value::Json` (serde) or `Value::Object` (no-serde) for JSON columns.
- Serializer accepts `Value::Json`, `Value::Object` (bytes), and `Value::String` (text).
- Inserts using `serde_json::Value` become `Value::Json` (serde) or `Value::Object` (no-serde), avoiding reparse in serializer.
- `values::json::Json<T>` now emits `Value::Json` under serde and fast-paths reads from `Value::Json`.
