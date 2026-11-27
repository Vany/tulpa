//! Whether this macOS can record system audio through a `CoreAudio` process tap (14.6 or later), read from
//! `/System/Library/CoreServices/SystemVersion.plist` without running anything.
//!
//! The parse is deliberately small: the `<string>` that follows `<key>ProductVersion</key>` in the XML plist, as
//! dot-separated numbers. Anything else — a missing file, a binary plist, no such key, a version that does not
//! parse — is refused with the reason, never guessed.

use std::fmt;
use std::path::Path;

/// Where macOS states its version.
pub const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";
/// The first macOS whose process taps `cpal` 0.18 can use.
pub const TAP_MIN: Version = Version {
    major: 14,
    minor: 6,
    patch: 0,
};

/// A macOS version; a missing minor or patch is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct Version {
    /// e.g. 14.
    pub major: u32,
    /// e.g. 6.
    pub minor: u32,
    /// e.g. 1.
    pub patch: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Parses `14`, `14.6` or `14.6.1`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split('.');
        let mut next = |required: bool| match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok(),
            None if !required => Some(0),
            _ => None,
        };
        let v = Self {
            major: next(true)?,
            minor: next(false)?,
            patch: next(false)?,
        };
        parts.next().is_none().then_some(v)
    }
}

/// Why the tap cannot run on this Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionError {
    /// The plist could not be read.
    Unreadable(String),
    /// The plist has no `ProductVersion` string, or it does not parse.
    Garbled,
    /// This macOS is older than [`TAP_MIN`].
    TooOld(Version),
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(e) => write!(
                f,
                "cannot read {SYSTEM_VERSION_PLIST} to check for macOS {TAP_MIN} or later: {e}"
            ),
            Self::Garbled => write!(
                f,
                "{SYSTEM_VERSION_PLIST} has no ProductVersion this can read; system audio needs macOS {TAP_MIN} or later"
            ),
            Self::TooOld(v) => write!(
                f,
                "macOS {v} cannot record system audio: the CoreAudio process tap needs macOS {TAP_MIN} or later"
            ),
        }
    }
}

impl std::error::Error for VersionError {}

/// The `ProductVersion` in an XML `SystemVersion.plist`.
pub fn product_version(plist: &str) -> Option<Version> {
    let (_, after) = plist.split_once("<key>ProductVersion</key>")?;
    let value = after.trim_start().strip_prefix("<string>")?;
    let (value, _) = value.split_once("</string>")?;
    Version::parse(value)
}

/// Checks a plist's text (or the error reading it) for [`TAP_MIN`].
///
/// # Errors
///
/// [`VersionError`] for an unreadable or garbled plist, or a macOS older than [`TAP_MIN`].
pub fn check_tap(plist: std::io::Result<String>) -> Result<Version, VersionError> {
    let text = plist.map_err(|e| VersionError::Unreadable(e.to_string()))?;
    let v = product_version(&text).ok_or(VersionError::Garbled)?;
    if v < TAP_MIN {
        return Err(VersionError::TooOld(v));
    }
    Ok(v)
}

/// [`check_tap`] on the file at `path` (normally [`SYSTEM_VERSION_PLIST`]).
///
/// # Errors
///
/// As [`check_tap`].
pub fn check_tap_at(path: &Path) -> Result<Version, VersionError> {
    check_tap(std::fs::read_to_string(path))
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests")]

    use super::*;

    fn plist(version: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>BuildID</key>\n\t\
             <string>ABC</string>\n\t<key>ProductBuildVersion</key>\n\t<string>23G80</string>\n\t\
             <key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductUserVisibleVersion</key>\n\t\
             <string>{version}</string>\n\t<key>ProductVersion</key>\n\t<string>{version}</string>\n</dict>\n</plist>\n"
        )
    }

    #[test]
    fn version_14_5_is_refused() {
        let e = check_tap(Ok(plist("14.5"))).unwrap_err();
        assert_eq!(
            e,
            VersionError::TooOld(Version {
                major: 14,
                minor: 5,
                patch: 0
            })
        );
        assert!(e.to_string().contains("14.6.0 or later"));
        assert!(check_tap(Ok(plist("13.7.4"))).is_err());
    }

    #[test]
    fn versions_from_14_6_are_accepted() {
        for v in ["14.6", "14.6.1", "15.0", "26.1", "27.2.1"] {
            assert!(check_tap(Ok(plist(v))).is_ok(), "{v}");
        }
        assert_eq!(
            check_tap(Ok(plist("27.2.1"))).unwrap(),
            Version {
                major: 27,
                minor: 2,
                patch: 1
            }
        );
    }

    #[test]
    fn missing_or_garbled_plist_is_refused_loudly() {
        let missing = check_tap(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        let e = missing.unwrap_err();
        assert!(matches!(e, VersionError::Unreadable(_)));
        assert!(e.to_string().contains(SYSTEM_VERSION_PLIST));
        for bad in [
            String::new(),
            "bplist00\u{1}\u{2}".to_owned(),
            plist("").replace("ProductVersion", "Nope"),
            plist("fourteen"),
            plist("14..6"),
            plist("14.6.1.9"),
            plist("-15"),
            "<key>ProductVersion</key><integer>15</integer>".to_owned(),
        ] {
            assert_eq!(
                check_tap(Ok(bad.clone())),
                Err(VersionError::Garbled),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn user_visible_version_is_not_mistaken_for_product_version() {
        let p = plist("15.1").replacen("<string>15.1</string>", "<string>99.0</string>", 1);
        assert_eq!(product_version(&p).unwrap().major, 15);
    }
}
