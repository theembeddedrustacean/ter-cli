//! Against the live site. Ignored by default; run with
//! `scripts/check.sh --live`, which needs `TER_TOKEN` for a test account.

use std::process::Command;

use serde_json::Value;

fn ter_json(token: &str, args: &[&str]) -> (bool, Value) {
    let config = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(args)
        .arg("--json")
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_TOKEN", token)
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(!stdout.contains(token), "the token must never be printed");
    (o.status.success(), serde_json::from_str(&stdout).unwrap())
}

#[test]
#[ignore = "live site"]
fn live_whoami_with_bad_token_is_token_invalid() {
    for bad in [
        "not-a-real-token",
        "0000000000000000000000000000000000000000",
    ] {
        let (ok, v) = ter_json(bad, &["whoami"]);
        assert!(!ok);
        assert_eq!(v["error"]["code"], "token_invalid", "{v}");
    }
}

#[test]
#[ignore = "live site"]
fn live_whoami_with_valid_token() {
    let token = std::env::var("TER_TOKEN").expect("TER_TOKEN must be set for live tests");
    let (ok, v) = ter_json(&token, &["whoami"]);
    assert!(ok, "{v}");
    assert!(v["user"].as_str().is_some_and(|u| u.contains('@')), "{v}");
    assert_eq!(v["cli_version"], env!("CARGO_PKG_VERSION"));
    assert!(v["min_supported_version"].is_string());
    assert_eq!(v["supported"], true);
    assert!(v["premium"].is_boolean(), "{v}");
}
