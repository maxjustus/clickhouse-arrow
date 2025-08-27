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
