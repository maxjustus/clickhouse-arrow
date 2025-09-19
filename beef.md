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
