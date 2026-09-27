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

const SITE: &str = "https://learn.theembeddedrustacean.com";

fn pair_call(method: &str, function: &str, query: &str) -> Value {
    let o = Command::new("curl")
        .args(["-sS", "-X", method, "-H", "Content-Type: application/json"])
        .args(if method == "POST" {
            &["-d", "{}"][..]
        } else {
            &[][..]
        })
        .arg(format!(
            "{SITE}/api/method/ter_courses.api.{function}{query}"
        ))
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    v["message"].clone()
}

#[test]
#[ignore = "live site"]
fn live_pair_start_gives_a_pending_code() {
    let start = pair_call("POST", "pair_start", "");
    let code = start["code"].as_str().expect("a code");
    let (letters, digits) = code.split_once('-').expect("XXXX-0000");
    assert!(
        letters.len() == 4 && digits.len() == 4,
        "unexpected code shape {code}"
    );
    assert_eq!(start["expires_in"], 600);
    let poll = pair_call("GET", "pair_poll", &format!("?code={code}"));
    assert_eq!(poll, serde_json::json!({"status": "pending"}));
}

#[test]
#[ignore = "live site"]
fn live_used_code_is_consumed() {
    // KFQY-5985 was approved and its token taken by `ter login` on
    // 2026-09-27; the site keeps answering `consumed` for it.
    let poll = pair_call("GET", "pair_poll", "?code=KFQY-5985");
    assert_eq!(poll, serde_json::json!({"status": "consumed"}));
}

#[test]
#[ignore = "live site"]
fn live_unknown_code_is_expired() {
    let poll = pair_call("GET", "pair_poll", "?code=ZZZZ-0000");
    assert_eq!(poll, serde_json::json!({"status": "expired"}));
}

/// Waits out a code's ten minutes; runs only with TER_LIVE_SLOW=1.
#[test]
#[ignore = "live site"]
fn live_code_left_ten_minutes_expires() {
    if std::env::var("TER_LIVE_SLOW").as_deref() != Ok("1") {
        eprintln!("skipped: set TER_LIVE_SLOW=1 to wait out a pairing code");
        return;
    }
    let config = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["login", "--no-browser", "--json"])
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_KEYCHAIN", "off")
        .env_remove("TER_TOKEN")
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["code"], "pairing_expired", "{v}");
    assert!(!config.path().join("token").exists());
}
