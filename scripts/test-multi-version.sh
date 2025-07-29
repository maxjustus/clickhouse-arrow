#!/usr/bin/env bash
# Note: Requires bash 4+ for associative arrays. On macOS: brew install bash

# Multi-version ClickHouse testing script
# Usage: ./scripts/test-multi-version.sh [versions...]
# Example: ./scripts/test-multi-version.sh 24.3 25.1 25.5 latest

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

# Test configuration - using non-standard ports to avoid conflicts
CLICKHOUSE_USER="${CLICKHOUSE_USER:-clickhouse}"
CLICKHOUSE_PASSWORD="${CLICKHOUSE_PASSWORD:-clickhouse}"
NATIVE_PORT="${NATIVE_PORT:-19000}"  # Standard is 9000, we use 19000
HTTP_PORT="${HTTP_PORT:-18123}"      # Standard is 8123, we use 18123
CONTAINER_NAME_PREFIX="clickhouse-test"

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

cleanup_container() {
    local container_name="$1"
    if docker ps -q -f name="$container_name" | grep -q .; then
        log_info "Stopping container $container_name..."
        docker stop "$container_name" >/dev/null 2>&1 || true
    fi
    if docker ps -aq -f name="$container_name" | grep -q .; then
        log_info "Removing container $container_name..."
        docker rm "$container_name" >/dev/null 2>&1 || true
    fi
}

start_clickhouse() {
    local version="$1"
    local container_name="$2"
    
    log_info "Starting ClickHouse $version..."
    
    # Clean up any existing container
    cleanup_container "$container_name"
    
    # Start new container
    docker run -d \
        --name "$container_name" \
        -p "$NATIVE_PORT:9000" \
        -p "$HTTP_PORT:8123" \
        -e CLICKHOUSE_USER="$CLICKHOUSE_USER" \
        -e CLICKHOUSE_PASSWORD="$CLICKHOUSE_PASSWORD" \
        "clickhouse/clickhouse-server:$version" >/dev/null
    
    # Wait for ClickHouse to be ready
    local timeout=60
    local ready=false
    
    while [ $timeout -gt 0 ]; do
        if docker logs "$container_name" 2>&1 | grep -q "Ready for connections"; then
            ready=true
            break
        fi
        sleep 2
        timeout=$((timeout - 2))
        echo -n "."
    done
    echo
    
    if [ "$ready" = false ]; then
        log_error "ClickHouse $version failed to start within timeout"
        docker logs "$container_name"
        return 1
    fi
    
    # Verify connection
    local actual_version
    actual_version=$(docker exec "$container_name" clickhouse-client --query "SELECT version()" 2>/dev/null || echo "unknown")
    log_success "ClickHouse $version started successfully (actual: $actual_version)"
}

run_test_suite() {
    local version="$1"
    local test_name="$2"
    local test_command="$3"
    
    log_info "Running $test_name for ClickHouse $version..."
    
    export CLICKHOUSE_VERSION="$version"
    export CLICKHOUSE_ENDPOINT="localhost"
    export CLICKHOUSE_NATIVE_PORT="$NATIVE_PORT"
    export CLICKHOUSE_HTTP_PORT="$HTTP_PORT"  
    export CLICKHOUSE_USER="$CLICKHOUSE_USER"
    export CLICKHOUSE_PASSWORD="$CLICKHOUSE_PASSWORD"
    export DISABLE_CLEANUP="true"
    export RUST_LOG="warn,clickhouse_arrow=debug"
    
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    
    if eval "$test_command" >/dev/null 2>&1; then
        log_success "$test_name passed"
        RESULTS["$version:$test_name"]="PASS"
        PASSED_TESTS=$((PASSED_TESTS + 1))
        return 0
    else
        log_warning "$test_name failed (may be expected for older versions)"
        RESULTS["$version:$test_name"]="FAIL"
        return 1
    fi
}

run_version_tests() {
    local version="$1"
    local container_name="$CONTAINER_NAME_PREFIX-$version"
    
    echo
    log_info "=== Testing ClickHouse $version ==="
    
    # Start ClickHouse
    if ! start_clickhouse "$version" "$container_name"; then
        log_error "Failed to start ClickHouse $version, skipping tests"
        return 1
    fi
    
    # Wait a bit for the container to fully initialize
    sleep 3
    
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
    
    # Cleanup
    cleanup_container "$container_name"
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
}

# Trap for cleanup on exit
cleanup_all() {
    log_info "Cleaning up all test containers..."
    for version in "${VERSIONS[@]}"; do
        cleanup_container "$CONTAINER_NAME_PREFIX-$version"
    done
}
trap cleanup_all EXIT

# Main execution
main() {
    log_info "Starting multi-version ClickHouse testing"
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
    
    # Exit with error if any tests failed
    if [ $PASSED_TESTS -lt $TOTAL_TESTS ]; then
        log_warning "Some tests failed or were skipped"
        exit 1
    else
        log_success "All tests passed!"
        exit 0
    fi
}

# Show help
if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
    echo "Multi-version ClickHouse testing script"
    echo
    echo "Usage: $0 [versions...]"
    echo
    echo "Examples:"
    echo "  $0                    # Test default versions: ${DEFAULT_VERSIONS[*]}"
    echo "  $0 24.3 25.5         # Test specific versions"
    echo "  $0 latest            # Test only latest version"
    echo
    echo "Environment variables:"
    echo "  CLICKHOUSE_USER      # Default: clickhouse"
    echo "  CLICKHOUSE_PASSWORD  # Default: clickhouse"
    echo "  NATIVE_PORT          # Default: 19000 (non-standard to avoid conflicts)"
    echo "  HTTP_PORT            # Default: 18123 (non-standard to avoid conflicts)"
    exit 0
fi

# Run main function
main "$@"