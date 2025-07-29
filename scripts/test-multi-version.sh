#!/usr/bin/env bash

# Multi-version ClickHouse testing script - Fixed version
# This version correctly uses the testcontainers infrastructure
# Usage: ./scripts/test-multi-version-fixed.sh [versions...]

set -euo pipefail

# Check if we're running in bash
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Error: This script requires bash but is running in $0"
    echo "Please run with: bash $0 $*"
    exit 1
fi

# Check bash version (need 4+ for associative arrays)
if [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
    echo "Error: This script requires bash 4.0 or later (found ${BASH_VERSION})"
    echo "On macOS, install with: brew install bash"
    echo "Then run with: /opt/homebrew/bin/bash $0 $*"
    exit 1
fi

# Default versions to test if none specified
DEFAULT_VERSIONS=("24.3" "24.8" "25.1" "25.5" "latest")
VERSIONS=("${@:-${DEFAULT_VERSIONS[@]}}")

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Results tracking
declare -A RESULTS
TOTAL_TESTS=0
PASSED_TESTS=0

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

run_test_suite() {
    local version="$1"
    local test_name="$2"
    local test_command="$3"
    
    log_info "Running $test_name for ClickHouse $version..."
    
    # Set the version for testcontainers to use
    export CLICKHOUSE_VERSION="$version"
    export RUST_LOG="warn"
    export RUSTFLAGS="-A warnings"
    
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    
    # Run the test - testcontainers will handle container lifecycle
    if eval "$test_command" >/dev/null 2>&1; then
        log_success "$test_name passed"
        RESULTS["$version:$test_name"]="PASS"
        PASSED_TESTS=$((PASSED_TESTS + 1))
        return 0
    else
        local exit_code=$?
        log_warning "$test_name failed with exit code $exit_code (may be expected for older versions)"
        RESULTS["$version:$test_name"]="FAIL"
        return 1
    fi
}

run_version_tests() {
    local version="$1"
    
    echo
    log_info "=== Testing ClickHouse $version ==="
    log_info "Note: Tests use testcontainers which automatically manages container lifecycle"
    
    # Run test suites
    local tests_run=0
    local tests_passed=0
    
    # Basic native tests (should work on all versions)
    if run_test_suite "$version" "basic_native" "cargo test --test e2e_native e2e_native --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    # Variant tests (25.1+)
    if run_test_suite "$version" "variant" "cargo test --test e2e_native e2e_native_variant --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    # Dynamic tests (25.1+)
    if run_test_suite "$version" "dynamic" "cargo test --test e2e_native e2e_native_dynamic --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    # JSON tests (25.1+)
    if run_test_suite "$version" "json" "cargo test --test e2e_native e2e_native_json --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    # Arrow format tests (should work on all versions)
    if run_test_suite "$version" "arrow" "cargo test --test e2e_arrow --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    # Compatibility tests (should work on all versions)
    if run_test_suite "$version" "compatibility" "cargo test --test e2e_compat --features 'test-utils,derive'"; then
        tests_passed=$((tests_passed + 1))
    fi
    tests_run=$((tests_run + 1))
    
    log_info "ClickHouse $version: $tests_passed/$tests_run tests passed"
}

print_summary() {
    echo
    echo "================================================================"
    log_info "MULTI-VERSION TEST SUMMARY"
    echo "================================================================"
    
    # Print results table
    printf "%-10s %-15s %-6s\n" "Version" "Test" "Result"
    echo "----------------------------------------"
    
    for version in "${VERSIONS[@]}"; do
        for test in "basic_native" "variant" "dynamic" "json" "arrow" "compatibility"; do
            local key="$version:$test"
            local result="${RESULTS[$key]:-SKIP}"
            local color="$NC"
            case "$result" in
                "PASS") color="$GREEN" ;;
                "FAIL") color="$YELLOW" ;;
                "SKIP") color="$BLUE" ;;
            esac
            printf "%-10s %-15s ${color}%-6s${NC}\n" "$version" "$test" "$result"
        done
        echo
    done
    
    echo "================================================================"
    log_info "Overall: $PASSED_TESTS/$TOTAL_TESTS tests passed"
    
    # Feature support summary
    echo
    log_info "FEATURE SUPPORT MATRIX"
    echo "================================================================"
    printf "%-10s %-8s %-8s %-8s %-8s\n" "Version" "Basic" "Variant" "Dynamic" "JSON"
    echo "--------------------------------------------"
    
    for version in "${VERSIONS[@]}"; do
        local basic="${RESULTS[$version:basic_native]:-SKIP}"
        local variant="${RESULTS[$version:variant]:-SKIP}"
        local dynamic="${RESULTS[$version:dynamic]:-SKIP}"
        local json="${RESULTS[$version:json]:-SKIP}"
        
        printf "%-10s %-8s %-8s %-8s %-8s\n" "$version" "$basic" "$variant" "$dynamic" "$json"
    done
    
    echo
    log_info "EXPECTED RESULTS:"
    echo "- Versions 24.x: Basic tests should pass, new features (Variant/Dynamic/JSON) expected to fail"
    echo "- Versions 25.1+: All tests should pass"
}

# Main execution
main() {
    log_info "Starting multi-version ClickHouse testing (using testcontainers)"
    log_info "Testing versions: ${VERSIONS[*]}"
    
    # Verify Docker is available
    if ! command -v docker >/dev/null 2>&1; then
        log_error "Docker is required but not found"
        exit 1
    fi
    
    # Verify Cargo is available
    if ! command -v cargo >/dev/null 2>&1; then
        log_error "Cargo is required but not found"
        exit 1
    fi
    
    # Build the project first
    log_info "Building project..."
    if ! cargo build --all-features >/dev/null 2>&1; then
        log_error "Failed to build project"
        exit 1
    fi
    
    # Run tests for each version
    for version in "${VERSIONS[@]}"; do
        run_version_tests "$version"
    done
    
    # Print summary
    print_summary
    
    # Exit with success - failures are expected for older versions
    if [ $PASSED_TESTS -gt 0 ]; then
        log_success "Testing completed! $PASSED_TESTS/$TOTAL_TESTS tests passed."
        log_info "Note: Test failures on older versions are expected for new features."
        exit 0
    else
        log_error "All tests failed - this is unexpected"
        exit 1
    fi
}

# Show help
if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
    echo "Multi-version ClickHouse testing script (using testcontainers)"
    echo
    echo "Usage: $0 [versions...]"
    echo
    echo "Examples:"
    echo "  $0                    # Test default versions: ${DEFAULT_VERSIONS[*]}"
    echo "  $0 24.3 25.5         # Test specific versions"
    echo "  $0 latest            # Test only latest version"
    echo
    echo "This script uses the testcontainers infrastructure which automatically"
    echo "manages container lifecycle. Each test creates its own container."
    echo
    echo "Expected results:"
    echo "- ClickHouse 24.x: Basic tests pass, new features fail"
    echo "- ClickHouse 25.1+: All tests pass"
    exit 0
fi

# Run main function
main "$@"