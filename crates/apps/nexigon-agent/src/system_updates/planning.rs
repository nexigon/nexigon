//! Planning update paths through checkpoint releases.
//!
//! A version may name a base version that a device must have run before it
//! installs the version. [`next_version`] decides which version a device
//! installs next on its way to a target.

use nexigon_api::types::repositories::PackageVersionId;

/// A version on the chain of bases from the target, with its own base.
#[derive(Debug, Clone)]
pub(crate) struct ChainEntry {
    pub version_id: PackageVersionId,
    pub base_version_id: Option<PackageVersionId>,
}

/// The running version, if it resolves to a package version.
#[derive(Debug, Clone)]
pub(crate) struct Running {
    pub version_id: PackageVersionId,
    pub base_version_id: Option<PackageVersionId>,
}

/// Version to install next on the way to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NextVersion {
    Install(PackageVersionId),
    /// The target is unreachable without going back across a checkpoint.
    NoUpdatePath,
    /// The target has a base, but the running version is unknown.
    UnknownCurrentVersion,
}

/// Decide which version to install next.
///
/// `chain` starts with the target and continues with the bases of the previous
/// entries until it reaches a version without a base. A device can install a
/// version directly if the version's base is the running version or both have
/// the same base, where two versions without a base count as having the same
/// base.
pub(crate) fn next_version(running: Option<&Running>, chain: &[ChainEntry]) -> NextVersion {
    let target = chain.first().expect("the chain starts with the target");
    let Some(running) = running else {
        return if target.base_version_id.is_none() {
            NextVersion::Install(target.version_id.clone())
        } else {
            NextVersion::UnknownCurrentVersion
        };
    };
    for candidate in chain {
        let base = candidate.base_version_id.as_ref();
        if base == Some(&running.version_id) || base == running.base_version_id.as_ref() {
            return NextVersion::Install(candidate.version_id.clone());
        }
    }
    NextVersion::NoUpdatePath
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Versions of the documented example: `1.2` and `1.4` without a base,
    /// 2.x based on `1.4`, and 3.x based on `2.2`.
    fn base_of(version: &str) -> Option<&'static str> {
        match version {
            "2.0" | "2.1" | "2.2" => Some("1.4"),
            "3.0" | "3.1" => Some("2.2"),
            _ => None,
        }
    }

    fn id(version: &str) -> PackageVersionId {
        format!("pkg_v_{:1>22}", version.replace('.', "x").replace('0', "o"))
            .parse()
            .unwrap()
    }

    fn chain(target: &str) -> Vec<ChainEntry> {
        let mut chain = Vec::new();
        let mut current = Some(target);
        while let Some(version) = current {
            chain.push(ChainEntry {
                version_id: id(version),
                base_version_id: base_of(version).map(id),
            });
            current = base_of(version);
        }
        chain
    }

    fn plan(running: Option<&str>, target: &str) -> NextVersion {
        let running = running.map(|version| Running {
            version_id: id(version),
            base_version_id: base_of(version).map(id),
        });
        next_version(running.as_ref(), &chain(target))
    }

    /// The paths of the documented example: devices pass through every
    /// checkpoint, may go back within a base, and refuse to go back across a
    /// checkpoint.
    #[test]
    fn documented_update_paths() {
        assert_eq!(plan(Some("1.2"), "3.1"), NextVersion::Install(id("1.4")));
        assert_eq!(plan(Some("1.4"), "3.1"), NextVersion::Install(id("2.2")));
        assert_eq!(plan(Some("2.2"), "3.1"), NextVersion::Install(id("3.1")));
        assert_eq!(plan(Some("2.0"), "2.2"), NextVersion::Install(id("2.2")));
        assert_eq!(plan(Some("2.1"), "3.1"), NextVersion::Install(id("2.2")));
        assert_eq!(plan(Some("3.1"), "3.0"), NextVersion::Install(id("3.0")));
        assert_eq!(plan(Some("3.1"), "2.1"), NextVersion::NoUpdatePath);
        assert_eq!(plan(Some("2.1"), "1.2"), NextVersion::NoUpdatePath);
    }

    /// An unknown running version can only install targets without a base.
    #[test]
    fn unknown_running_version() {
        assert_eq!(plan(None, "1.2"), NextVersion::Install(id("1.2")));
        assert_eq!(plan(None, "2.0"), NextVersion::UnknownCurrentVersion);
    }
}
