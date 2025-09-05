#![allow(unused_crate_dependencies)]

pub mod common;
pub mod tests;

const TRACING_DIRECTIVES: &[(&str, &str)] = &[("testcontainers", "debug")];

// Test native e2e
#[cfg(feature = "derive")]
e2e_test!(e2e_native, tests::native::test_round_trip, TRACING_DIRECTIVES, None);

// Test variant e2e
#[cfg(feature = "derive")]
e2e_test!(e2e_native_variant, tests::native::test_variant_round_trip, TRACING_DIRECTIVES, None);

// Test dynamic e2e
#[cfg(feature = "derive")]
e2e_test!(e2e_native_dynamic, tests::native::test_dynamic_round_trip, TRACING_DIRECTIVES, None);

// Test JSON e2e
#[cfg(feature = "derive")]
e2e_test!(e2e_native_json, tests::native::test_json_round_trip, TRACING_DIRECTIVES, None);

// Test JSON with Variant typed paths e2e
#[cfg(feature = "derive")]
e2e_test!(
    e2e_native_json_typed_paths_variant,
    tests::native::test_json_typed_paths_variant,
    TRACING_DIRECTIVES,
    None
);

// Test JSON with arrays e2e
#[cfg(feature = "derive")]
e2e_test!(e2e_native_json_arrays, tests::native::test_json_arrays, TRACING_DIRECTIVES, None);

// Test mixed Dynamic and JSON e2e
#[cfg(feature = "derive")]
e2e_test!(
    e2e_native_mixed_dynamic_json,
    tests::native::test_mixed_dynamic_json,
    TRACING_DIRECTIVES,
    None
);

// Test evil heterogeneous arrays in Dynamic e2e
#[cfg(feature = "derive")]
e2e_test!(
    e2e_native_evil_heterogeneous,
    tests::native::test_evil_heterogeneous_dynamic,
    TRACING_DIRECTIVES,
    None
);

// Test evil heterogeneous arrays in JSON e2e
#[cfg(feature = "derive")]
e2e_test!(
    e2e_native_evil_heterogeneous_json,
    tests::native::test_evil_heterogeneous_json,
    TRACING_DIRECTIVES,
    None
);

// InsertInto (streaming) e2e tests
#[cfg(feature = "derive")]
e2e_test!(
    e2e_insert_into_nonjson_multi,
    tests::native::test_insert_into_nonjson_multi,
    TRACING_DIRECTIVES,
    None
);

#[cfg(feature = "derive")]
e2e_test!(
    e2e_insert_into_mixed_json,
    tests::native::test_insert_into_mixed_json,
    TRACING_DIRECTIVES,
    None
);

#[cfg(feature = "derive")]
e2e_test!(
    e2e_json_minimal_insert_into,
    tests::native::test_json_minimal_insert_into,
    TRACING_DIRECTIVES,
    None
);

// Test direct serde JSON/Object deserialization for JSON columns
#[cfg(feature = "derive")]
e2e_test!(
    e2e_json_direct_serde,
    tests::native::test_json_direct_deserialize,
    TRACING_DIRECTIVES,
    None
);

#[cfg(feature = "derive")]
e2e_test!(
    e2e_json_direct_dynamic,
    tests::native::test_json_direct_dynamic,
    TRACING_DIRECTIVES,
    None
);

#[cfg(feature = "serde")]
e2e_test!(e2e_query_json_basic, tests::native::test_query_json_basic, TRACING_DIRECTIVES, None);

#[cfg(feature = "serde")]
e2e_test!(e2e_json_map_roundtrip, tests::native::test_json_map_roundtrip, TRACING_DIRECTIVES, None);
