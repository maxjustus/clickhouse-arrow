use std::str::FromStr;

use tracing::{debug, warn};

/// `ClickHouse` version information
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self { Self { major, minor, patch } }

    /// Parse version string like "24.3.5.46" or "25.1.2.3-testing"
    pub fn parse(version_str: &str) -> Option<Self> {
        // Split on whitespace and take first part to handle versions like "25.1.2.3 (official
        // build)"
        let version_part = version_str.split_whitespace().next()?;

        // Split on dots and take first 3 parts
        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.len() < 2 {
            return None;
        }

        let major = parts[0].parse().ok()?;
        let minor = parts[1].parse().ok()?;
        let patch = if parts.len() >= 3 {
            // Handle cases like "2-testing"
            parts[2].split('-').next()?.parse().ok()?
        } else {
            0
        };

        Some(Version::new(major, minor, patch))
    }

    /// Check if this version supports the Dynamic type (requires 24.8+)
    pub fn supports_dynamic(&self) -> bool { self >= &Version::new(24, 8, 0) }

    /// Check if this version supports the JSON type (requires 25.1+)
    pub fn supports_json(&self) -> bool { self >= &Version::new(25, 1, 0) }

    /// Check if this version supports JSON v3 object serialization (requires 25.6+)
    pub fn supports_json_v3(&self) -> bool { self >= &Version::new(25, 6, 0) }

    /// Check if this version supports Variant type (requires 25.1+)
    pub fn supports_variant(&self) -> bool { self >= &Version::new(25, 1, 0) }

    /// Check if this version has stable Dynamic/JSON support (requires 25.6+)
    pub fn has_stable_dynamic_json(&self) -> bool { self >= &Version::new(25, 6, 0) }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("Invalid version format: {s}"))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Version compatibility checker for tests
#[derive(Copy, Clone)]
pub struct VersionChecker {
    version: Option<Version>,
}

impl VersionChecker {
    pub fn new(version_str: Option<&str>) -> Self {
        let version = version_str.and_then(Version::parse);
        if let Some(ref v) = version {
            debug!("Detected ClickHouse version: {}", v);
        } else if let Some(version_str) = version_str {
            warn!("Failed to parse ClickHouse version: {}", version_str);
        }
        Self { version }
    }

    pub fn version(&self) -> Option<&Version> { self.version.as_ref() }

    /// Generic method to check feature support and skip test if not available
    fn require_feature<F>(
        &self,
        test_name: &str,
        feature_name: &str,
        min_version: &str,
        check: F,
    ) -> bool
    where
        F: Fn(&Version) -> bool,
    {
        match &self.version {
            Some(v) if check(v) => true,
            Some(v) => {
                warn!(
                    "Skipping {} - requires {} (ClickHouse {}+), found {}",
                    test_name, feature_name, min_version, v
                );
                false
            }
            None => {
                warn!("Skipping {} - could not determine ClickHouse version", test_name);
                false
            }
        }
    }

    /// Skip test if version doesn't support the feature
    pub fn require_dynamic_support(&self, test_name: &str) -> bool {
        self.require_feature(test_name, "Dynamic support", "24.8", Version::supports_dynamic)
    }

    pub fn require_json_support(&self, test_name: &str) -> bool {
        self.require_feature(test_name, "JSON support", "25.1", Version::supports_json)
    }

    pub fn require_json_v3_support(&self, test_name: &str) -> bool {
        self.require_feature(test_name, "JSON v3 support", "25.6", Version::supports_json_v3)
    }

    pub fn require_variant_support(&self, test_name: &str) -> bool {
        self.require_feature(test_name, "Variant support", "25.1", Version::supports_variant)
    }

    pub fn require_stable_dynamic_json(&self, test_name: &str) -> bool {
        self.require_feature(
            test_name,
            "stable Dynamic/JSON support",
            "25.6",
            Version::has_stable_dynamic_json,
        )
    }

    /// Log version compatibility information
    pub fn log_compatibility_info(&self) {
        if let Some(v) = &self.version {
            debug!(
                "ClickHouse {} features: Dynamic({}), JSON({}), JSON-v3({}), Variant({}), \
                 Stable-Dynamic/JSON({})",
                v,
                v.supports_dynamic(),
                v.supports_json(),
                v.supports_json_v3(),
                v.supports_variant(),
                v.has_stable_dynamic_json()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parsing() {
        assert_eq!(Version::parse("24.3.5.46"), Some(Version::new(24, 3, 5)));
        assert_eq!(Version::parse("25.1.2.3"), Some(Version::new(25, 1, 2)));
        assert_eq!(Version::parse("25.6.0"), Some(Version::new(25, 6, 0)));
        assert_eq!(Version::parse("25.1.2.3-testing"), Some(Version::new(25, 1, 2)));
        assert_eq!(Version::parse("25.1.2.3 (official build)"), Some(Version::new(25, 1, 2)));

        // Invalid formats
        assert_eq!(Version::parse("invalid"), None);
        assert_eq!(Version::parse("24"), None);
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn test_feature_support() {
        let v24_3 = Version::new(24, 3, 0);
        let v25_1 = Version::new(25, 1, 0);
        let v25_6 = Version::new(25, 6, 0);

        // 24.3 - no new features
        assert!(!v24_3.supports_dynamic());
        assert!(!v24_3.supports_json());
        assert!(!v24_3.supports_json_v3());
        assert!(!v24_3.supports_variant());
        assert!(!v24_3.has_stable_dynamic_json());

        // 25.1 - experimental support
        assert!(v25_1.supports_dynamic());
        assert!(v25_1.supports_json());
        assert!(!v25_1.supports_json_v3()); // v3 requires 25.6+
        assert!(v25_1.supports_variant());
        assert!(!v25_1.has_stable_dynamic_json());

        // 25.6 - stable support
        assert!(v25_6.supports_dynamic());
        assert!(v25_6.supports_json());
        assert!(v25_6.supports_json_v3());
        assert!(v25_6.supports_variant());
        assert!(v25_6.has_stable_dynamic_json());
    }

    #[test]
    fn test_version_comparison() {
        let v24_3 = Version::new(24, 3, 0);
        let v25_1 = Version::new(25, 1, 0);
        let v25_6 = Version::new(25, 6, 0);

        assert!(v24_3 < v25_1);
        assert!(v25_1 < v25_6);
        assert!(v25_6 > v25_1);
        assert!(v25_1 > v24_3);
    }
}
