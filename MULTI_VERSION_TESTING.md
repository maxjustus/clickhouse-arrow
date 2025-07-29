# Multi-Version ClickHouse Testing

This document describes the multi-version testing infrastructure for the `clickhouse-arrow` project.

## Overview

The project supports testing against multiple versions of ClickHouse to ensure compatibility across different server versions. This is particularly important because newer ClickHouse types like `Dynamic`, `JSON`, and `Variant` are only available in recent versions (25.1+).

## Version Support Matrix

| Feature | 24.3 | 24.8 | 25.1 | 25.5 | latest |
|---------|------|------|------|------|--------|
| Basic Types | ✅ | ✅ | ✅ | ✅ | ✅ |
| Arrays/Maps | ✅ | ✅ | ✅ | ✅ | ✅ |
| Variant | ❌ | ❌ | ⚠️ | ✅ | ✅ |
| Dynamic | ❌ | ❌ | ⚠️ | ✅ | ✅ |
| JSON v3 | ❌ | ❌ | ⚠️ | ✅ | ✅ |

- ✅ Full support
- ⚠️ Experimental/limited support
- ❌ Not supported

## Testing Infrastructure

### 1. Version Compatibility Checker

The `version_compat.rs` module provides:
- Version parsing and comparison
- Feature support detection
- Test skipping for unsupported features

```rust
use crate::common::version_compat::VersionChecker;

let version_checker = VersionChecker::new(Some("25.5.1.2"));
if !version_checker.require_json_support("JSON test") {
    return; // Skip test
}
```

### 2. GitHub Actions Multi-Version Matrix

The `.github/workflows/multi-version-test.yml` workflow:
- Tests against multiple ClickHouse versions automatically
- Generates compatibility reports
- Runs on push/PR/manual dispatch
- Uses Docker containers for different versions

```yaml
matrix:
  clickhouse-version:
    - "24.3"    # LTS version
    - "24.8"    # Stable version  
    - "25.1"    # First Dynamic/JSON support
    - "25.5"    # Stable Dynamic/JSON support
    - "latest"  # Latest features
```

### 3. Local Multi-Version Testing Script

The `scripts/test-multi-version.sh` script allows local testing:

```bash
# Test default versions (24.3, 24.8, 25.1, 25.5, latest)
./scripts/test-multi-version.sh

# Test specific versions
./scripts/test-multi-version.sh 25.1 25.5 latest

# Test only latest
./scripts/test-multi-version.sh latest
```

**Important:** This script requires bash 4.0+ for associative arrays. On macOS:
```bash
# Install modern bash
brew install bash

# Run the script with modern bash
/opt/homebrew/bin/bash ./scripts/test-multi-version.sh
```

## Running Multi-Version Tests

### Prerequisites

- Docker installed and running
- Rust toolchain with cargo
- Bash 4.0 or later (for test scripts)
  - macOS system bash is 3.2, install modern bash with: `brew install bash`

### Local Testing

1. **Run all versions:**
   ```bash
   # On Linux or with modern bash
   ./scripts/test-multi-version.sh
   
   # On macOS with system bash
   /opt/homebrew/bin/bash ./scripts/test-multi-version.sh
   ```

2. **Run specific versions:**
   ```bash
   ./scripts/test-multi-version.sh 25.5 latest
   ```

3. **How it works:**
   - The script uses the existing testcontainers infrastructure
   - Each test creates and manages its own ephemeral ClickHouse container
   - The `CLICKHOUSE_VERSION` environment variable controls which version is used
   - No manual container management is required

### GitHub Actions

The workflow runs automatically on:
- Push to `main` or `type-improvements` branches
- Pull requests to `main`
- Manual dispatch with custom version selection

## Test Categories by Version

### All Versions (24.3+)
- Basic type serialization/deserialization
- Array and Map types
- Arrow format compatibility
- Connection and query functionality

### Version 25.1+
- Variant type support (experimental)
- Dynamic type support (experimental)
- JSON type support (experimental)

### Version 25.5+
- Stable Variant type support
- Stable Dynamic type support
- JSON v3 object serialization
- Full flattened serialization support

## Version-Specific Test Behavior

Tests automatically detect the ClickHouse version and:
1. **Skip unsupported features** with clear warning messages
2. **Log version compatibility information** for debugging
3. **Adjust expectations** based on feature maturity

Example test output:
```
[WARN] Skipping JSON type test - requires JSON support (ClickHouse 25.1+), found 24.3.5
[DEBUG] ClickHouse 25.5.1 feature support:
  Dynamic type: true
  JSON type: true
  JSON v3 serialization: true
  Variant type: true
  Stable Dynamic/JSON: true
```

## Adding New Version-Dependent Features

When adding support for new ClickHouse features:

1. **Update version compatibility checker:**
   ```rust
   // In version_compat.rs
   pub fn supports_new_feature(&self) -> bool {
       self >= &Version::new(25, 6, 0) // Adjust version as needed
   }
   ```

2. **Add version check in tests:**
   ```rust
   if !version_checker.require_new_feature_support("New feature test") {
       return;
   }
   ```

3. **Update documentation:**
   - Add feature to version support matrix
   - Update GitHub Actions workflow if needed
   - Document any special considerations

## Troubleshooting

### Common Issues

1. **Bash version error:**
   ```bash
   Error: This script requires bash 4.0 or later (found 3.2.57(1)-release)
   ```
   Solution: Use modern bash
   ```bash
   # Install on macOS
   brew install bash
   
   # Run with modern bash
   /opt/homebrew/bin/bash ./scripts/test-multi-version.sh
   ```

2. **Test failures on older versions:**
   - This is expected! Older versions don't support new features
   - Version 24.x: Only basic tests should pass
   - Version 25.1+: All tests should pass
   - The script will show a feature support matrix at the end

3. **All tests showing as failed:**
   - Make sure you're using the correct bash version
   - The script uses testcontainers - each test manages its own container
   - Check that Docker is running and accessible

### Debug Mode

Enable debug logging:
```bash
RUST_LOG=debug ./scripts/test-multi-version.sh
```

## Contributing

When contributing changes that affect version compatibility:

1. Test against multiple versions locally
2. Update version compatibility checks if needed
3. Ensure CI passes for all target versions
4. Document any version-specific behavior

The multi-version testing ensures that the library maintains compatibility across the ClickHouse ecosystem while taking advantage of new features in recent versions.