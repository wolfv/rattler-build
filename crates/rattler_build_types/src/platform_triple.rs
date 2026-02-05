//! Platform triple type for cross-compilation scenarios.

use rattler_conda_types::Platform;
use serde::{Deserialize, Serialize};

/// Represents the three platforms involved in a cross-compilation scenario.
///
/// In conda's build system:
/// - `build`: The platform where the build is running
/// - `host`: The platform where the package will be installed (host dependencies run here)
/// - `target`: The platform for which the package is being built (e.g., cross-compiling a compiler)
///
/// For native builds, all three are typically the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformTriple {
    /// The platform where the build process runs
    pub build: Platform,
    /// The platform where the package will be installed
    pub host: Platform,
    /// The platform the package is being built for
    pub target: Platform,
}

impl PlatformTriple {
    /// Create a new `PlatformTriple` with all platforms set to the current platform.
    pub fn native() -> Self {
        let current = Platform::current();
        Self {
            build: current,
            host: current,
            target: current,
        }
    }

    /// Create a new `PlatformTriple` with the given platforms.
    pub fn new(build: Platform, host: Platform, target: Platform) -> Self {
        Self {
            build,
            host,
            target,
        }
    }

    /// Returns true if this is a cross-compilation scenario (build != target).
    pub fn is_cross_compilation(&self) -> bool {
        self.build != self.target
    }
}

impl Default for PlatformTriple {
    fn default() -> Self {
        Self::native()
    }
}
