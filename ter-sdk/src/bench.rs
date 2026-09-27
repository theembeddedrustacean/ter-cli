//! The bench registry on the site: `ter serve --share` registers a bench
//! and keeps it alive, its owner shares it under a code, and someone else
//! connects with that code. The site only holds who owns a bench, whether
//! it is alive and who may drive it; runs go straight to the bench.

use serde::{Deserialize, Serialize};

/// `bench_register`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Registered {
    pub bench: String,
    pub board: String,
    pub label: String,
    /// Seconds between heartbeats.
    pub heartbeat_every: u64,
}

/// `bench_heartbeat`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Heartbeat {
    pub bench: String,
    pub status: String,
    /// Whether sharing is on; the owner may have turned it off on the
    /// Devices page or with `ter bench unshare`.
    pub sharing: bool,
    /// The code in force; `None` while sharing is off.
    #[serde(default)]
    pub share_code: Option<String>,
    /// The names of the people connected now.
    #[serde(default)]
    pub connected: Vec<String>,
}

/// One of this account's own benches, as the site lists them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Mine {
    pub name: String,
    pub label: String,
    pub board: String,
    #[serde(default)]
    pub url: Option<String>,
    /// `available`, `busy` or `offline`.
    pub status: String,
    #[serde(default)]
    pub sharing: bool,
    #[serde(default)]
    pub share_code: Option<String>,
    /// The names of the people connected now.
    #[serde(default)]
    pub connected: Vec<String>,
}

/// `bench_share`'s answer: the code to hand out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Shared {
    pub bench: String,
    pub share_code: String,
}

/// `bench_unshare`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Unshared {
    pub bench: String,
    /// How many connected people were dropped.
    #[serde(default)]
    pub dropped: usize,
}

/// `bench_connect` and `bench_reconnect`: the bench now in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    pub bench: String,
    pub label: String,
    pub board: String,
    pub owner_name: String,
    /// `available`, `busy` or `offline`.
    pub status: String,
    /// Where the owner's `ter serve` takes runs; none until it registers
    /// one.
    #[serde(default)]
    pub url: Option<String>,
}

/// The `{ok, bench}` answer of disconnect, forget and remove.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Done {
    pub bench: String,
}

/// A share code as the site writes it (`WXYZ-1234`), from what someone
/// typed: case, spaces and a missing dash forgiven. `None` if it cannot be
/// one.
pub fn normalise_code(code: &str) -> Option<String> {
    let flat: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let (letters, digits) = flat.split_at_checked(4)?;
    (letters.chars().all(|c| c.is_ascii_uppercase())
        && digits.len() == 4
        && digits.chars().all(|c| c.is_ascii_digit()))
    .then(|| format!("{letters}-{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_forgiven_their_typing() {
        assert_eq!(normalise_code("GZTS-2520").as_deref(), Some("GZTS-2520"));
        assert_eq!(normalise_code(" gzts 2520 ").as_deref(), Some("GZTS-2520"));
        assert_eq!(normalise_code("gzts2520").as_deref(), Some("GZTS-2520"));
        assert_eq!(normalise_code("GZT-2520"), None);
        assert_eq!(normalise_code("GZTS-252"), None);
        assert_eq!(normalise_code("1234-ABCD"), None);
        assert_eq!(normalise_code("ÉZTS-2520"), None);
    }

    #[test]
    fn a_heartbeat_says_whether_sharing_is_on_and_who_is_connected() {
        let on: Heartbeat = serde_json::from_str(
            r#"{"ok": true, "bench": "b1", "status": "busy", "sharing": true,
                "share_code": "GZTS-2520", "connected": ["Ana", "Ben"]}"#,
        )
        .unwrap();
        assert!(on.sharing);
        assert_eq!(on.share_code.as_deref(), Some("GZTS-2520"));
        assert_eq!(on.connected, ["Ana", "Ben"]);
        let off: Heartbeat = serde_json::from_str(
            r#"{"ok": true, "bench": "b1", "status": "available", "sharing": false,
                "share_code": null, "connected": []}"#,
        )
        .unwrap();
        assert!(!off.sharing);
        assert_eq!(off.share_code, None);
    }
}
