//! The add-on API version: the one number that decides whether a component
//! may run in a host.
//!
//! The add-on API is the WIT package `farever:addon@MAJOR.MINOR.PATCH`. The SDK
//! mirrors it in `farever-more-sdk`'s `WIT_PACKAGE` constant, a component's
//! imports name every interface at that version
//! (`farever:addon/dependencies@1.0.0`), and a packed manifest declares it as
//! `api-version`. All of those are the same value; `scripts/sync-addon-sdk.ps1`
//! and `scripts/check-addon-sdk-boundary.ps1` keep them equal.
//!
//! ```text
//! MAJOR  breaking: the previous major's components are refused, not adapted
//! MINOR  additive:  components built against an older minor keep running
//! PATCH  fixes:     no contract change, compatible in both directions
//! ```
//!
//! The rule is not a convention the two sides may disagree about: the
//! Component Model resolves imports by versioned interface name, and Wasmtime
//! matches those names within a semver *track* (the major, or `major.minor`
//! while the major is zero). [`compatibility`] therefore accepts exactly the
//! pairs the linker can satisfy — anything it refuses would otherwise surface
//! as an unreadable instantiation error.

use std::fmt;

/// The add-on API version this build of the framework implements.
///
/// Equal to the WIT package version in `wit/farever-addon.wit`; the boundary
/// check fails the build when the two drift apart.
pub const ADDON_API_VERSION: &str = "1.0.0";

/// A parsed `MAJOR.MINOR.PATCH` add-on API version.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ApiVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl ApiVersion {
    /// Parses the exact `MAJOR.MINOR.PATCH` form used by the WIT package and
    /// the manifest. Partial versions (`1`, `1.2`) and prerelease tags are
    /// rejected: an ABI that is not pinned to a patch release cannot be
    /// compared, and a prerelease is never compatible with anything.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parse_component(parts.next()?)?;
        let patch = parse_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    pub const fn major(self) -> u64 {
        self.major
    }

    pub const fn minor(self) -> u64 {
        self.minor
    }

    pub const fn patch(self) -> u64 {
        self.patch
    }

    /// The version this build of the framework implements.
    ///
    /// Panics only if [`ADDON_API_VERSION`] is not a literal version, which the
    /// boundary check already rejects.
    #[must_use]
    pub fn host() -> Self {
        Self::parse(ADDON_API_VERSION).expect("ADDON_API_VERSION is a semantic version")
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn parse_component(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if value.len() > 1 && value.starts_with('0') {
        return None;
    }
    value.parse().ok()
}

/// Checks that a component requiring `required` may run on a host implementing
/// `provided`.
///
/// Returns the reason the pair cannot work as a message that names both
/// versions and what to do about it; callers add the add-on's identity.
pub fn compatibility(required: ApiVersion, provided: ApiVersion) -> Result<(), String> {
    if required.major != provided.major {
        return Err(format!(
            "add-on API {required} is not compatible with runtime API {provided}: \
             major versions make no compatibility guarantee, so the add-on has to be \
             rebuilt for API {provided}"
        ));
    }
    if provided.major == 0 {
        // While the API is at 0.x the minor release is the changing part, so
        // only its own minor track links.
        if required.minor != provided.minor {
            return Err(format!(
                "add-on API {required} is not compatible with runtime API {provided}: \
                 a 0.x API is only valid within its own minor version, so the add-on has \
                 to be rebuilt for API {provided}"
            ));
        }
        return Ok(());
    }
    if required.minor > provided.minor {
        return Err(format!(
            "add-on API {required} needs a newer runtime: this runtime implements API \
             {provided}, so update the framework instead"
        ));
    }
    // Any patch is compatible in both directions: a patch release never changes
    // the contract, so an add-on may be built for a newer or older one.
    Ok(())
}

/// Parses a version that came from a manifest or a component, naming the source
/// in the error message.
pub fn parse_api_version(value: &str, source: &str) -> Result<ApiVersion, String> {
    ApiVersion::parse(value).ok_or_else(|| {
        format!("{source} must be a MAJOR.MINOR.PATCH add-on API version, found {value:?}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> ApiVersion {
        ApiVersion::parse(value).expect("parse version")
    }

    #[test]
    fn versions_parse_and_round_trip() {
        assert_eq!(version("1.0.0").to_string(), "1.0.0");
        assert_eq!(version(" 12.4.9 ").to_string(), "12.4.9");
        assert_eq!(version("1.0.0").major(), 1);
        assert_eq!(version("1.3.7").minor(), 3);
        assert_eq!(version("1.3.7").patch(), 7);
    }

    #[test]
    fn partial_and_malformed_versions_are_rejected() {
        for value in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.3-rc.1",
            "1.2.x",
            "01.2.3",
            "v1.2.3",
        ] {
            assert_eq!(ApiVersion::parse(value), None, "{value:?}");
        }
        assert!(
            ApiVersion::parse("1.2.3").is_some(),
            "the full form is accepted"
        );
    }

    #[test]
    fn the_host_version_is_a_literal_version() {
        assert_eq!(ApiVersion::host().to_string(), ADDON_API_VERSION);
    }

    #[test]
    fn older_minors_and_any_patch_run_on_a_newer_runtime() {
        let host = version("1.4.2");
        assert!(compatibility(version("1.4.2"), host).is_ok());
        assert!(compatibility(version("1.0.0"), host).is_ok());
        assert!(compatibility(version("1.3.99"), host).is_ok());
        assert!(compatibility(version("1.4.0"), host).is_ok());
    }

    #[test]
    fn patch_releases_are_compatible_in_both_directions() {
        assert!(compatibility(version("1.4.9"), version("1.4.2")).is_ok());
        assert!(compatibility(version("1.4.2"), version("1.4.9")).is_ok());
    }

    #[test]
    fn a_newer_minor_needs_a_newer_runtime() {
        let error = compatibility(version("1.5.0"), version("1.4.2")).expect_err("newer minor");
        assert!(
            error.contains("1.5.0") && error.contains("1.4.2"),
            "{error}"
        );
        assert!(error.contains("newer runtime"), "{error}");
    }

    #[test]
    fn major_versions_make_no_guarantee() {
        for (required, provided) in [("2.0.0", "1.4.2"), ("1.4.2", "2.0.0"), ("3.1.0", "2.9.9")] {
            let error = compatibility(version(required), version(provided)).expect_err("major");
            assert!(error.contains("major versions"), "{error}");
        }
    }

    #[test]
    fn a_zero_major_api_is_only_compatible_within_its_minor() {
        assert!(compatibility(version("0.15.3"), version("0.15.0")).is_ok());
        assert!(compatibility(version("0.15.0"), version("0.15.9")).is_ok());
        let error = compatibility(version("0.14.0"), version("0.15.0")).expect_err("minor");
        assert!(
            error.contains("0.14.0") && error.contains("0.15.0"),
            "{error}"
        );
        assert!(error.contains("own minor"), "{error}");
    }

    #[test]
    fn parsing_a_declared_version_names_its_source() {
        let error = parse_api_version("0.15", "addon.json api-version").expect_err("partial");
        assert!(error.contains("addon.json api-version"), "{error}");
        assert!(error.contains("MAJOR.MINOR.PATCH"), "{error}");
    }
}
