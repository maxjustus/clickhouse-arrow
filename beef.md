- for v1/v2 JSON serialization would it make any sense to hold the count of dynamic paths in memory over the course of multiple insert blocks?

- question:
  - when inserting into a JSON column that has predefined types for specific paths do we need to properly handle that on the client?
    if yes - do we already?
- For example,  JsonSerializer  still has its own  group_values_by_type ,  write_discriminators_internal_... , and  
     other helpers that are almost identical to the ones in  DynamicSerializer .                                         
- deal with deprecated JSON fields - refactor to fully reuse Dynamic functionality and get rid of duplicative code
- we should generate golden files from --format Variant queries to verify how we're serializing to match clickhouse
  or just run commands directly and diff?

Next up, I can correct the JSON typed-path handling so that for non-nullable typed paths we substitute default values instead of Null (so we don’t
attempt to inject Nulls into non-nullable LC or other non-nullable types). Want me to implement that?

codex mentioned something about including the one off e2e tests via macro in the main e2e test files. We should do that.

do we have any facility to or should we implement the ability to decode JSON column data we select directly into a Serde Object?

- [ ] That convenience helper idea is compelling but limited in scope. I'm thinking about clickhouse client handles this by parsing and inferring/
      coercing input format data. The fact that we're using Rust gives us Serde which is a huge advantage for this use case.
      An API which allows a user to insert a Serde object and have the library map it to the insert table clickhouse types automatically would be amazing.
      We could possibly expand on or reuse what we're doing for insert type inferrence with the JSON col type for this. What do you think?


Insert serde api:
- [ ] Expand coercion to more complex non‑JSON types (Date/DateTime, Arrays, Tuples, Maps, Variant non‑JSON).
- [ ] Add strict=true behavior to reject lossy conversions and enforce exact type matches.
- Optionally use insert_many to reduce insert handshakes across multiple batches and only close the stream once at finish().

Confirm that we don't already have an API like this that we're duplicating

- [ ] explore improvements for streaming insert:
  - Keep insert session open across flushes:
      - For streaming InsertInto, holding the insert open and reusing the first Header avoids a header fetch per flush. This is a small improvement to latency without changing the API surface.

- [ ] explore serde json for decoding JSON cols dynamically without knowing types in advance
- [ ] explore Serde map Object for decoding rows as well
- [ ] examine including serde object encoding for Arrow format as well

- [ ] remove sync serialize/deserialize code now that we have async compressed reads/writes working.
  - [ ] stress test compressed reads/writes with large datasets

- [ ] add a random structure round trip test that compares output / input against clickhouse client
- [ ] add golden file type tests

- we now have sparse state paths for deserialization. Does it make sense to somehow unify that with type specific state?
- kind_plan is maybe not a great name. serialization_type_by_path is more descriptive?

- what's up with this?
............2025-09-12T23:04:33.766007Z  WARN e2e_native::common::version_compat: clickhouse-arrow/tests/common/version_compat.r
s:110: Skipping test_json_direct_dynamic - could not determine ClickHouse version

why to_json_with_type instead of just to_json?

- what if we parsed and presented profile event values both as raw numbers and as human readable strings relevant to each event type/unit? or, add extra top level values that are the human readable versions or give different scales
like seconds instead of microseconds, MB instead of bytes, etc.

- would be cool if we could detect server version and automatically serialize json cols using v1 (string), or v3 flattened. Same for setting the output format native settings for one vs the other?

- Add example / test idea of using Native format over stdin/stdout for executable UDF. Should be able to test using clickhouse local.

- TODO: for profile events and progress events we need to present cumulative values in addition to deltas.
  The server delivers delta from last event delivery (first event is "delta" from zero). We need to keep a running total and present that as well.
  Maybe some other stats could be useful too.. Computed rates, etc?

- clickhouse-arrow/src/native/block.rs format_type_for_header is useless. Remove
- remembering what I've done:
  - sparse serialization handling
  - Variant handling
  - Named tuple handling
  - Nested type handling
  - Dynamic handling (v3 flattened format)
  - JSON col handling (v1 string format, v3 flattened format)
  - Test client
  - Add UInt256 sized serializer support

- check on behavior of wrap_heterogeneous_elements and needs_heterogeneous_transformation in the native array serializer. Should this be handled at a higher level? Kinda feels like maybe
- make sure we have roundrip named tuple and nested tests. Plus UInt256 serialization test.
- add generateRandomStructure based tests to fuzz test serialize and deserialize.
- add read Native dump from clickhouse client and write Native dump to clickhouse client tests

- JSON insert via pipe idea if it doesn't already work this way:
  - start an insert with an identifier in the CLI and a timeout by pushing a json object saying { action: "insert", database: "db", table: "table", timeout: n, session_id: "uuid" }
  - then stream json objects to stdin
  - then send a final json object { action: "finish", session_id: "uuid" }
  - the client should batch up json objects and send them as Native format insert blocks to the server. It could also connect to the server async while reading stdin and batching.
    - if it did that then you could do away with the fancy session api and just start an insert, close the CLI when you're done.
- save / display host / connection string used for query

- other thought: we can query system.query_log when query starts to get more metadata about the query like tables, functions, etc.

- idea: right pane (row inspect) column focus follows selected column in the table view and vice versa.

- row value pane search
- unobtrusive column list
- easy "pivot" view of of tables for cases where there tons of columns
- need an easier way to inspect schemas/available functions, settings.. both via like a searchable tree view and via autocomplete in the query editor
  - what if tree view was populated with everything and typing filtered down per tree as I type?
  - filtering should handle patterns like db_name.table.col.* , etc.
