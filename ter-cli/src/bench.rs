//! `ter bench` and `ter connect`: sharing a bench and using someone
//! else's.
//!
//! The site keeps the registry; this machine keeps what it needs to reach
//! a bench again in `benches.json` in the config directory (mode 600: it
//! holds share codes): the benches it serves, and the ones this account
//! connected to, newest first. `ter run --hw --venue bench` uses the
//! newest.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::json;
use ter_sdk::bench::{Connected, normalise_code};

use crate::config::config_dir;
use crate::output::{CliError, print_json};
use crate::session::Session;

pub const BENCHES_FILE: &str = "benches.json";

/// A bench this machine serves with `ter serve --share`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Served {
    pub site: String,
    pub user: String,
    pub bench: String,
    pub board: String,
    pub label: String,
}

/// A bench this account connected to with `ter connect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub site: String,
    pub user: String,
    pub bench: String,
    pub code: String,
    pub label: String,
    pub board: String,
    pub owner_name: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Benches {
    #[serde(default)]
    pub served: Vec<Served>,
    /// Newest first.
    #[serde(default)]
    pub connected: Vec<Link>,
}

fn path() -> Result<PathBuf, CliError> {
    Ok(config_dir()?.join(BENCHES_FILE))
}

impl Benches {
    pub fn load() -> Result<Self, CliError> {
        let path = path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                CliError::new(
                    "config_error",
                    format!("{} is not valid: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::new(
                "config_error",
                format!("Could not read {}: {e}", path.display()),
            )),
        }
    }

    pub fn save(&self) -> Result<(), CliError> {
        let text = serde_json::to_string_pretty(self).expect("benches serialise");
        crate::token_store::write_private(&path()?, &text)
            .map_err(|e| CliError::new("io_error", e.message))
    }

    /// Remember a bench this machine serves, replacing an older record of
    /// it.
    pub fn serve(&mut self, served: Served) {
        self.served
            .retain(|s| !(s.site == served.site && s.bench == served.bench));
        self.served.insert(0, served);
    }

    /// The bench this machine last served for `label`, to update rather
    /// than register anew.
    pub fn served_as(&self, site: &str, user: &str, label: &str) -> Option<&Served> {
        self.served
            .iter()
            .find(|s| s.site == site && s.user == user && s.label == label)
    }

    /// Remember a connection, newest first.
    pub fn connect(&mut self, link: Link) {
        self.connected
            .retain(|l| !(l.site == link.site && l.user == link.user && l.bench == link.bench));
        self.connected.insert(0, link);
    }

    /// The newest bench `user` connected to on `site`.
    pub fn current(&self, site: &str, user: &str) -> Option<&Link> {
        self.connected
            .iter()
            .find(|l| l.site == site && l.user == user)
    }
}

/// The one of `names` (bench name, label) that `asked` means, or the only
/// one when nothing is asked.
fn pick<'a, T>(
    items: &'a [T],
    asked: Option<&str>,
    names: impl Fn(&T) -> (&str, &str),
    what: &str,
) -> Result<&'a T, CliError> {
    let listed = || {
        items
            .iter()
            .map(|i| {
                let (name, label) = names(i);
                format!("{name} ({label})")
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match asked {
        Some(a) => items
            .iter()
            .find(|i| {
                let (name, label) = names(i);
                name == a || label == a
            })
            .ok_or_else(|| {
                CliError::new(
                    "not_found",
                    if items.is_empty() {
                        format!("No {what} called {a:?}.")
                    } else {
                        format!("No {what} called {a:?}; there is {}.", listed())
                    },
                )
            }),
        None => match items {
            [] => Err(CliError::new("not_found", format!("No {what} yet."))),
            [one] => Ok(one),
            _ => Err(CliError::new(
                "bench_required",
                format!("Say which {what}: {}.", listed()),
            )),
        },
    }
}

/// The session and the account it posts as.
async fn signed_in() -> Result<(Session, String), CliError> {
    let session = Session::open()?;
    let user = session
        .client
        .ping()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?
        .user
        .clone();
    Ok((session, user))
}

/// One of this account's own benches, by name or label.
async fn own(session: &Session, asked: Option<&str>) -> Result<ter_sdk::bench::Mine, CliError> {
    let mine = session
        .client
        .my_benches()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    pick(&mine, asked, |b| (&b.name, &b.label), "bench of yours").cloned()
}

pub async fn list(json: bool) -> Result<(), CliError> {
    let (session, user) = signed_in().await?;
    let site = session.client.site_url().to_string();
    let mine = session
        .client
        .my_benches()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    let benches = Benches::load()?;
    let connected: Vec<&Link> = benches
        .connected
        .iter()
        .filter(|l| l.site == site && l.user == user)
        .collect();
    if json {
        let mine: Vec<_> = mine
            .iter()
            .map(|b| {
                json!({"bench": b.name, "label": b.label, "board": b.board, "status": b.status,
                       "url": b.url, "sharing": b.sharing, "share_code": b.share_code,
                       "connected": b.connected})
            })
            .collect();
        let connected: Vec<_> = connected
            .iter()
            .map(|l| {
                json!({"bench": l.bench, "label": l.label, "board": l.board,
                       "owner_name": l.owner_name, "url": l.url})
            })
            .collect();
        print_json(&json!({"mine": mine, "connected": connected}));
        return Ok(());
    }
    if mine.is_empty() {
        println!("You have no benches. Share your board with `ter serve --share --board <id>`.");
    } else {
        println!("Your benches:");
        for b in &mine {
            let sharing = match (&b.sharing, &b.share_code) {
                (true, Some(code)) => format!("shared as {code}"),
                _ => "not shared".into(),
            };
            let who = if b.connected.is_empty() {
                String::new()
            } else {
                format!(", connected: {}", b.connected.join(", "))
            };
            println!(
                "  {}  {} ({}), {}, {sharing}{who}",
                b.name, b.label, b.board, b.status
            );
        }
    }
    if !connected.is_empty() {
        println!("Benches you connected to (ter run --hw --venue bench uses the first):");
        for l in connected {
            println!(
                "  {}  {} ({}), {}'s",
                l.bench, l.label, l.board, l.owner_name
            );
        }
    }
    Ok(())
}

pub async fn share(bench: Option<String>, json: bool) -> Result<(), CliError> {
    let (session, _) = signed_in().await?;
    let b = own(&session, bench.as_deref()).await?;
    let shared = session
        .client
        .bench_share(&b.name)
        .await
        .map_err(CliError::from)?;
    if json {
        print_json(&json!({"bench": shared.bench, "share_code": shared.share_code}));
    } else {
        println!(
            "{} is shared as {}. Whoever has the code runs `ter connect {}`.",
            b.label, shared.share_code, shared.share_code
        );
    }
    Ok(())
}

pub async fn unshare(bench: Option<String>, json: bool) -> Result<(), CliError> {
    let (session, _) = signed_in().await?;
    let b = own(&session, bench.as_deref()).await?;
    let done = session
        .client
        .bench_unshare(&b.name)
        .await
        .map_err(CliError::from)?;
    if json {
        print_json(&json!({"bench": done.bench, "dropped": done.dropped}));
    } else {
        println!(
            "{} is no longer shared; its code stopped working and {} connected {} dropped.",
            b.label,
            done.dropped,
            if done.dropped == 1 {
                "person was"
            } else {
                "people were"
            }
        );
    }
    Ok(())
}

pub async fn remove(bench: Option<String>, json: bool) -> Result<(), CliError> {
    let (session, _) = signed_in().await?;
    let b = own(&session, bench.as_deref()).await?;
    session
        .client
        .bench_remove(&b.name)
        .await
        .map_err(CliError::from)?;
    let mut benches = Benches::load()?;
    let site = session.client.site_url();
    benches
        .served
        .retain(|s| !(s.site == site && s.bench == b.name));
    benches.save()?;
    if json {
        print_json(&json!({"bench": b.name, "removed": true}));
    } else {
        println!("Removed {} and every share of it.", b.label);
    }
    Ok(())
}

pub async fn connect(code: &str, json: bool) -> Result<(), CliError> {
    let code = normalise_code(code).ok_or_else(|| {
        CliError::new(
            "not_found",
            format!("{code:?} is not a share code. It looks like WXYZ-1234."),
        )
    })?;
    let (session, user) = signed_in().await?;
    let c = session
        .client
        .bench_connect(&code)
        .await
        .map_err(CliError::from)?;
    let mut benches = Benches::load()?;
    benches.connect(link(&session, &user, &code, &c));
    benches.save()?;
    if json {
        print_json(&c);
    } else {
        println!(
            "Connected to {} ({}), {}'s bench, {} now.",
            c.label, c.board, c.owner_name, c.status
        );
        println!("Run an exercise on it with `ter run --hw --venue bench`.");
    }
    Ok(())
}

pub fn link(session: &Session, user: &str, code: &str, c: &Connected) -> Link {
    Link {
        site: session.client.site_url().to_string(),
        user: user.to_string(),
        bench: c.bench.clone(),
        code: code.to_string(),
        label: c.label.clone(),
        board: c.board.clone(),
        owner_name: c.owner_name.clone(),
        url: c.url.clone(),
    }
}

/// A bench this account connected to, by name or label; the newest when
/// not named.
fn connected_one(
    benches: &Benches,
    site: &str,
    user: &str,
    asked: Option<&str>,
) -> Result<Link, CliError> {
    let mine: Vec<Link> = benches
        .connected
        .iter()
        .filter(|l| l.site == site && l.user == user)
        .cloned()
        .collect();
    match asked {
        None => mine.into_iter().next().ok_or_else(|| {
            CliError::new(
                "not_found",
                "You have not connected to a bench. Run `ter connect <code>` with the owner's code.",
            )
        }),
        Some(_) => pick(&mine, asked, |l| (&l.bench, &l.label), "connected bench").cloned(),
    }
}

pub async fn disconnect(bench: Option<String>, forget: bool, json: bool) -> Result<(), CliError> {
    let (session, user) = signed_in().await?;
    let site = session.client.site_url().to_string();
    let mut benches = Benches::load()?;
    let l = connected_one(&benches, &site, &user, bench.as_deref())?;
    if forget {
        session.client.bench_forget(&l.bench).await
    } else {
        session.client.bench_disconnect(&l.bench).await
    }
    .map_err(CliError::from)?;
    if forget {
        benches
            .connected
            .retain(|c| !(c.site == site && c.user == user && c.bench == l.bench));
        benches.save()?;
    }
    if json {
        print_json(&json!({"bench": l.bench, "forgotten": forget}));
    } else if forget {
        println!("Forgot {}; connect again with a new code.", l.label);
    } else {
        println!(
            "Disconnected from {}. `ter connect {}` connects again while the owner shares it.",
            l.label, l.code
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(bench: &str, user: &str) -> Link {
        Link {
            site: "https://s".into(),
            user: user.into(),
            bench: bench.into(),
            code: "GZTS-2520".into(),
            label: format!("{bench} label"),
            board: "xiao-esp32c3".into(),
            owner_name: "Owner".into(),
            url: None,
        }
    }

    #[test]
    fn the_newest_connection_is_the_current_one_per_account() {
        let mut b = Benches::default();
        b.connect(link("b1", "ana"));
        b.connect(link("b2", "ana"));
        b.connect(link("b3", "ben"));
        assert_eq!(b.current("https://s", "ana").unwrap().bench, "b2");
        b.connect(link("b1", "ana"));
        assert_eq!(b.current("https://s", "ana").unwrap().bench, "b1");
        assert_eq!(b.connected.len(), 3, "reconnecting does not duplicate");
        assert_eq!(b.current("https://other", "ana"), None);
        assert_eq!(
            connected_one(&b, "https://s", "ana", Some("b2 label"))
                .unwrap()
                .bench,
            "b2"
        );
        assert_eq!(
            connected_one(&b, "https://s", "cy", None).unwrap_err().code,
            "not_found"
        );
    }

    #[test]
    fn a_bench_is_picked_by_name_or_label_or_alone() {
        let items = [("b1", "Desk"), ("b2", "Lab")];
        fn names<'a>(i: &'a (&'static str, &'static str)) -> (&'a str, &'a str) {
            (i.0, i.1)
        }
        assert_eq!(pick(&items, Some("Lab"), names, "bench").unwrap().0, "b2");
        assert_eq!(pick(&items, Some("b1"), names, "bench").unwrap().0, "b1");
        let err = pick(&items, None, names, "bench").unwrap_err();
        assert_eq!(err.code, "bench_required");
        assert!(
            err.message.contains("b1 (Desk), b2 (Lab)"),
            "{}",
            err.message
        );
        assert_eq!(pick(&items[..1], None, names, "bench").unwrap().0, "b1");
        assert_eq!(
            pick(&items, Some("x"), names, "bench").unwrap_err().code,
            "not_found"
        );
    }
}
