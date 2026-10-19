//! Semantic version value + tag parsing for the update check. Hand-rolled (no `semver` crate):
//! our tags are simple `vMAJOR.MINOR.PATCH`, and a pre-release suffix is treated as "not a
//! stable release" rather than ordered.

/// A `MAJOR.MINOR.PATCH` version. `Ord` is the natural field order, so comparison answers
/// "is one release newer than another".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse `MAJOR.MINOR.PATCH` with an optional leading `v`. Returns `None` for anything that
    /// is not exactly three numeric components — including pre-release/build suffixes
    /// (`1.2.3-rc1`), which must never count as a stable release.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None; // more than three components
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse the running build's own package version. Unlike a remote tag, the build may carry
/// semver *build metadata* (`1.18.0+session`: a fork build of 1.18.0 that must compile from
/// source), which does not affect precedence and is dropped before parsing. A pre-release
/// suffix (`-rc1`) still yields `None`, as for tags.
pub fn own_version(s: &str) -> Option<Version> {
    let base = s.split_once('+').map_or(s, |(base, _metadata)| base);
    Version::parse(base)
}

/// The version compiled into this binary (Cargo's package version).
pub fn current() -> Version {
    // The package version is always a valid triple plus optional build metadata (CI/clippy would
    // reject otherwise), so this parse cannot fail in a real build; fall back to 0.0.0 rather than
    // panic, keeping the "never crash" invariant even for a hand-mangled version string.
    own_version(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// `Some(latest)` iff `latest` is strictly newer than the running build, else `None`.
pub fn newer_than_current(latest: Version) -> Option<Version> {
    (latest > current()).then_some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_version_drops_build_metadata_but_not_prerelease() {
        let v = Version {
            major: 1,
            minor: 17,
            patch: 0,
        };
        assert_eq!(own_version("1.17.0+session"), Some(v));
        assert_eq!(own_version("1.17.0"), Some(v));
        assert_eq!(own_version("1.17.0-rc1"), None);
        // A remote tag carrying metadata is still not a stable release.
        assert_eq!(Version::parse("1.17.0+session"), None);
        // The compiled-in version always resolves (never the 0.0.0 fallback).
        assert_ne!(
            current(),
            Version {
                major: 0,
                minor: 0,
                patch: 0
            }
        );
    }

    #[test]
    fn parses_plain_and_v_prefixed_triples() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            Version::parse("v1.0.0"),
            Some(Version {
                major: 1,
                minor: 0,
                patch: 0
            })
        );
    }

    #[test]
    fn rejects_non_triples_and_prereleases() {
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("1.2.x"), None);
        assert_eq!(Version::parse("1.2.3-rc1"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v"), None);
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        assert!(Version::parse("1.1.0").unwrap() > Version::parse("1.0.9").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.9.9").unwrap());
        assert!(Version::parse("1.0.10").unwrap() > Version::parse("1.0.9").unwrap());
        assert_eq!(
            Version::parse("1.0.0").unwrap(),
            Version::parse("v1.0.0").unwrap()
        );
    }

    #[test]
    fn displays_as_a_dotted_triple() {
        assert_eq!(
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
            .to_string(),
            "1.2.3"
        );
    }

    #[test]
    fn newer_than_current_compares_against_the_build_version() {
        let cur = current();
        // The current version is never "newer" than itself.
        assert_eq!(newer_than_current(cur), None);
        let higher = Version {
            major: cur.major + 1,
            ..cur
        };
        assert_eq!(newer_than_current(higher), Some(higher));
    }
}
