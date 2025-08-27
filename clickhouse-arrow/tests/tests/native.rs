use std::sync::Arc;

use clickhouse_arrow::CompressionMethod;
use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;

use crate::common::native_helpers::*;

// Helper struct for type check queries
#[derive(Debug, Clone, Row)]
#[allow(dead_code)]
struct TypeCheckRow {
    dtype: String,
}

// Helper functions to reduce repetitive patterns

/// # Panics
pub async fn test_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_compression(CompressionMethod::LZ4);
    let block = generate_test_block();

    harness
        .run_native_roundtrip_test("test_round_trip", &block)
        .await
        .expect("Round trip failed");
}

/// # Panics
pub async fn test_variant_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_compression(CompressionMethod::LZ4);
    let block = generate_variant_test_block();

    harness
        .run_native_roundtrip_test("test_variant_round_trip", &block)
        .await
        .expect("Variant round trip failed");
}

/// Tests dynamic type round-trip serialization.
///
/// # Panics
/// Panics if the dynamic round trip test fails.
pub async fn test_dynamic_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_dynamic_test_block();

    harness
        .run_native_roundtrip_test("test_dynamic_round_trip", &block)
        .await
        .expect("Dynamic round trip failed");
}

/// Tests JSON type round-trip serialization.
///
/// # Panics
/// Panics if the JSON round trip test fails.
pub async fn test_json_round_trip(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_json_test_block();

    harness
        .run_native_roundtrip_test("test_json_round_trip", &block)
        .await
        .expect("JSON round trip failed");
}

/// Tests mixed dynamic and JSON type round-trip serialization.
///
/// # Panics
/// Panics if the mixed dynamic JSON round trip test fails.
pub async fn test_mixed_dynamic_json(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_mixed_dynamic_json_test_block();

    harness
        .run_native_roundtrip_test("test_mixed_dynamic_json", &block)
        .await
        .expect("Mixed dynamic JSON round trip failed");
}

/// Tests evil heterogeneous arrays in dynamic type round-trip serialization.
///
/// # Panics
/// Panics if the evil heterogeneous dynamic round trip test fails.
pub async fn test_evil_heterogeneous_dynamic(ch: Arc<ClickHouseContainer>) {
    let harness = NativeRoundtripTestHarness::new(&ch).with_v3_format();
    let block = generate_evil_heterogeneous_dynamic_test_block();

    harness
        .run_native_roundtrip_test("test_evil_heterogeneous_dynamic", &block)
        .await
        .expect("Evil heterogeneous dynamic round trip failed");
}
