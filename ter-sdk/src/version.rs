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

#[cfg(test)]
mod tests {
    use super::*;

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
