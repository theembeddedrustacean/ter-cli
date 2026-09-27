//! The site's `min_supported_version` gate.

use semver::Version;

use crate::Error;

/// Refuse when `current` is below the site's `minimum`.
///
/// A minimum the site sends that is not a version is ignored rather than
/// locking every learner out.
pub fn check_supported(current: &str, minimum: &str) -> Result<(), Error> {
    let (Ok(cur), Ok(min)) = (Version::parse(current), Version::parse(minimum.trim())) else {
        return Ok(());
    };
    if cur < min {
        return Err(Error::Outdated {
            current: current.to_string(),
            minimum: minimum.trim().to_string(),
        });
    }
    Ok(())
}

/// `latest` when it is newer than `current`; `None` when it is not, or when
/// either is not a version.
pub fn newer_available(current: &str, latest: &str) -> Option<String> {
    let cur = Version::parse(current).ok()?;
    let new = Version::parse(latest.trim()).ok()?;
    (new > cur).then(|| new.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_latest_is_offered() {
        assert_eq!(newer_available("0.1.0", "0.1.1"), Some("0.1.1".into()));
        assert_eq!(newer_available("0.1.9", "0.1.10"), Some("0.1.10".into()));
        assert_eq!(newer_available("0.2.0-rc.1", "0.2.0"), Some("0.2.0".into()));
    }

    #[test]
    fn same_or_older_latest_is_not_offered() {
        assert_eq!(newer_available("0.1.0", "0.1.0"), None);
        assert_eq!(newer_available("0.2.0", "0.1.10"), None);
    }

    #[test]
    fn unparsable_latest_is_not_offered() {
        assert_eq!(newer_available("0.1.0", ""), None);
        assert_eq!(newer_available("0.1.0", "soon"), None);
    }

    #[test]
    fn below_minimum_is_outdated() {
        let err = check_supported("0.0.9", "0.1.0").unwrap_err();
        assert_eq!(err.code(), "outdated");
        assert!(err.to_string().contains("ter self-update"));
    }

    #[test]
    fn equal_or_above_is_supported() {
        assert!(check_supported("0.1.0", "0.1.0").is_ok());
        assert!(check_supported("0.1.1", "0.1.0").is_ok());
        assert!(check_supported("1.0.0", "0.9.12").is_ok());
    }

    #[test]
    fn compares_numerically_not_as_text() {
        assert!(check_supported("0.1.9", "0.1.10").is_err());
        assert!(check_supported("0.1.10", "0.1.9").is_ok());
    }

    #[test]
    fn prerelease_is_below_its_release() {
        assert!(check_supported("0.2.0-rc.1", "0.2.0").is_err());
    }

    #[test]
    fn unparsable_minimum_does_not_block() {
        assert!(check_supported("0.1.0", "").is_ok());
        assert!(check_supported("0.1.0", "latest").is_ok());
    }
}
