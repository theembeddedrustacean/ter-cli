//! `ter login` and `ter logout`.
//!
//! Pairing never asks for a password: `ter` gets a short code from the
//! site, the learner approves it on the site's `/cli` page where they are
//! already signed in, and the site hands `ter` a device token once.

use std::time::Duration;

use serde::Serialize;
use ter_sdk::pairing::{Next, Poller};

use crate::output::{CliError, print_json};
use crate::session::Session;
use crate::token_store::Location;

#[derive(Serialize)]
struct LoggedIn<'a> {
    user: &'a str,
    site: &'a str,
    stored_in: &'static str,
    token_file: Option<String>,
}

#[derive(Serialize)]
struct LoggedOut {
    removed_from: Vec<&'static str>,
}

pub async fn login(session: &Session, json: bool, open_browser: bool) -> Result<(), CliError> {
    let client = &session.client;
    let start = client.pair_start().await?;
    let url = client.pairing_url(&start.code);

    // With --json, stdout carries only the result; the prompt is for a human.
    let say = |text: &str| {
        if json {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
    };
    say(&pairing_prompt(&url, &start.code, start.expires_in));
    if open_browser && has_display() {
        // Best effort: the URL is printed either way.
        let _ = webbrowser::open(&url);
    }

    let mut poller = Poller::new(Duration::from_secs(start.expires_in));
    let token = loop {
        match poller.next(client.pair_poll(&start.code).await) {
            Next::Wait(d) => tokio::time::sleep(d).await,
            Next::Done(token) => break token,
            Next::Fail(e) => return Err(e.into()),
        }
    };

    let saved = session.store.save(&token)?;
    if let Some(reason) = &saved.keychain_error
        && let Location::File(path) = &saved.location
    {
        eprintln!(
            "warning: no system keychain is available ({reason}), so the token is in {}, readable only by you.",
            path.display()
        );
    }
    if session.token_source == "TER_TOKEN" {
        eprintln!("note: TER_TOKEN is set, and takes the place of this token until it is unset.");
    }

    let paired = client.with_token(token);
    let ping = paired.ping().await?;
    if json {
        print_json(&LoggedIn {
            user: &ping.user,
            site: client.site_url(),
            stored_in: saved.location.source(),
            token_file: match &saved.location {
                Location::File(p) => Some(p.display().to_string()),
                Location::Keychain => None,
            },
        });
    } else {
        let place = match &saved.location {
            Location::Keychain => "the system keychain".to_string(),
            Location::File(p) => p.display().to_string(),
        };
        println!(
            "Logged in as {}. The device token is in {place}.",
            ping.user
        );
    }
    Ok(())
}

pub fn logout(session: &Session, json: bool) -> Result<(), CliError> {
    let removed = session.store.clear()?;
    if json {
        print_json(&LoggedOut {
            removed_from: removed.iter().map(Location::source).collect(),
        });
        return Ok(());
    }
    if removed.is_empty() {
        println!("This machine was not logged in.");
    }
    for location in &removed {
        match location {
            Location::Keychain => println!("Removed the device token from the system keychain."),
            Location::File(p) => println!("Removed the device token from {}.", p.display()),
        }
    }
    if !removed.is_empty() {
        println!("To cut it off on the site too, revoke this device on your Devices page.");
    }
    if session.token_source == "TER_TOKEN" {
        eprintln!("note: TER_TOKEN is still set in this shell.");
    }
    Ok(())
}

fn pairing_prompt(url: &str, code: &str, expires_in: u64) -> String {
    let minutes = expires_in.div_ceil(60);
    let lasts = match minutes {
        0 | 1 => "a minute".to_string(),
        m => format!("{m} minutes"),
    };
    format!(
        "To pair this machine with your TER Learn account, open\n\n    {url}\n\n\
         sign in if asked, and approve this code:\n\n    {code}\n\n\
         The code lasts {lasts}. Waiting for approval..."
    )
}

/// Whether a browser window can be opened here. Over SSH or on a headless
/// Linux box there is none, and trying could start a text-mode browser.
fn has_display() -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_shows_the_url_and_the_code_on_their_own_lines() {
        let p = pairing_prompt(
            "https://learn.example.com/cli?code=WXYZ-1234",
            "WXYZ-1234",
            600,
        );
        assert!(
            p.contains("\n    https://learn.example.com/cli?code=WXYZ-1234\n"),
            "{p}"
        );
        assert!(p.contains("\n    WXYZ-1234\n"), "{p}");
        assert!(p.contains("10 minutes"), "{p}");
    }

    #[test]
    fn prompt_rounds_the_lifetime_up() {
        let p = |s| pairing_prompt("u", "c", s);
        assert!(p(599).contains("10 minutes"));
        assert!(p(61).contains("2 minutes"));
        assert!(p(45).contains("a minute"));
    }
}
