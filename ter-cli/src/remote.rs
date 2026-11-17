//! The bench venue for `ter run --hw --venue bench`: the board of someone
//! who shares it, reached through their `ter serve --share`.
//!
//! Settled before the build, like the local venue: the site says whether
//! this account may still use the bench and where it is, and the bench
//! itself says whether it answers, which board it has and whether someone
//! else is driving. Any of that failing posts nothing.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ter_flash::Tool;
use ter_remote::bench::{BenchClient, BenchError, BenchInfo, RemoteBench, ToolSpec, fits};
use ter_sdk::bench::Using;
use ter_sdk::project::TerToml;

use crate::bench::{Benches, Link};
use crate::output::CliError;
use crate::session::Session;

pub struct BenchReady {
    pub link: Link,
    pub url: String,
    pub info: BenchInfo,
    client: BenchClient,
    tool: ToolSpec,
}

impl BenchReady {
    pub fn venue(&self, ter: &TerToml, timeout_ms: u64, run_dir: PathBuf) -> RemoteBench {
        RemoteBench::new(
            self.client.clone(),
            &self.info,
            &ter.target,
            self.tool.clone(),
            timeout_ms,
            run_dir,
        )
    }
}

fn unavailable(message: impl Into<String>) -> CliError {
    CliError::new("venue_unavailable", message)
}

fn from_bench(e: BenchError) -> CliError {
    let code = match e.code.as_str() {
        "bench_offline" | "bench_busy" | "wrong_board" => e.code.as_str(),
        _ => "venue_unavailable",
    };
    CliError::new(code, e.message)
}

/// How often a busy bench is asked again while a run waits for it.
const BUSY_POLL: Duration = Duration::from_secs(2);

/// Which bench, and how long to wait for it when someone else is driving.
#[derive(Debug, Clone, Default)]
pub struct Want {
    /// The bench's name (as the Devices list gives it) or label; `None`
    /// is the one this account connected to last.
    pub bench: Option<String>,
    /// How long to wait while another driver holds the bench before
    /// answering `bench_busy`; zero answers at once.
    pub wait: Duration,
}

/// The bench `want` names (or the one this account connected to last),
/// ready for a run of `ter`'s exercise flashed with `tool`.
pub async fn ready(
    session: &Session,
    user: &str,
    ter: &TerToml,
    tool: &Tool,
    want: &Want,
) -> Result<BenchReady, CliError> {
    let tool = spec(tool)?;
    let site = session.client.site_url().to_string();
    let mut benches = Benches::load()?;
    let saved = match want.bench.as_deref() {
        None => benches.current(&site, user).cloned(),
        Some(asked) => benches
            .connected
            .iter()
            .find(|l| l.site == site && l.user == user && (l.bench == asked || l.label == asked))
            .cloned(),
    };
    // Not on this machine (connected on the Devices page, say): the site
    // knows the bench and, while this account is connected, its code.
    let mut link = match saved {
        Some(link) => link,
        None => {
            let using = using(session).await?;
            let row = match want.bench.as_deref() {
                Some(asked) => using
                    .iter()
                    .find(|u| !u.own && (u.name == asked || u.label == asked)),
                None => using
                    .iter()
                    .filter(|u| !u.own && u.connected)
                    .find(|u| u.share_code.is_some())
                    .or_else(|| using.iter().find(|u| !u.own && u.connected)),
            };
            match (row, want.bench.as_deref()) {
                (Some(row), _) => from_row(session, user, row)?,
                (None, Some(asked)) => {
                    return Err(CliError::new(
                        "unknown_bench",
                        format!(
                            "{asked} is not in your list of benches. Enter its share code on the Devices page, or run `ter connect <code>`."
                        ),
                    ));
                }
                (None, None) => {
                    return Err(unavailable(
                        "You are not connected to a bench. Ask its owner for the share code and enter it on the Devices page, or run `ter connect <code>`.",
                    ));
                }
            }
        }
    };
    let connected = session
        .client
        .bench_reconnect(&link.bench)
        .await
        .map_err(|e| match e.code() {
            "locked" => unavailable(format!(
                "{} is not sharing {} right now.",
                link.owner_name, link.label
            )),
            "not_found" => unavailable(format!(
                "{} is no longer in your list of benches. Enter its share code on the Devices page, or run `ter connect <code>`.",
                link.label
            )),
            _ => session.forget_dead_token(e.into()),
        })?;
    if connected.url.is_some() {
        link.url = connected.url.clone();
    }
    if let Some(code) = connected.share_code.clone() {
        link.code = code;
    }
    benches.connect(link.clone());
    benches.save()?;
    let mut url = link.url.clone().ok_or_else(|| {
        unavailable(format!(
            "{}'s bench {} has no address yet: its owner has not started `ter serve --share`.",
            link.owner_name, link.label
        ))
    })?;

    let mut client = BenchClient::new(&url, &link.code, user).map_err(from_bench)?;
    // A code the bench refuses is an old one: the owner shared again under
    // a new code. Ask the site for the current one, once.
    match client.info().await {
        Err(e) if matches!(e.code.as_str(), "share_code_refused" | "not_sharing") => {
            let fresh = using(session)
                .await?
                .into_iter()
                .find(|u| u.name == link.bench);
            match fresh {
                Some(row) if row.share_code.is_some() => {
                    let row = from_row(session, user, &row)?;
                    if row.code == link.code {
                        return Err(from_bench(e));
                    }
                    link.code = row.code;
                    if let Some(u) = row.url {
                        link.url = Some(u.clone());
                        url = u;
                    }
                    benches.connect(link.clone());
                    benches.save()?;
                    client = BenchClient::new(&url, &link.code, user).map_err(from_bench)?;
                }
                Some(row) => return Err(from_row(session, user, &row).unwrap_err()),
                // An older site: what the bench said stands.
                None => return Err(from_bench(e)),
            }
        }
        _ => {}
    }
    let asked_at = Instant::now();
    let info = loop {
        let info = client.info().await.map_err(|e| {
            let mut err = from_bench(e);
            if err.code == "bench_offline" {
                err.message = format!(
                    "{}'s bench {} is offline: {}",
                    link.owner_name, link.label, err.message
                );
            }
            err
        })?;
        if !fits(&info.board, &ter.target) {
            return Err(CliError::new(
                "wrong_board",
                format!(
                    "{} is a {}, and {} is for {}.",
                    link.label, info.board, ter.exercise, ter.target
                ),
            ));
        }
        let Some(driver) = info.driver.clone().filter(|d| d != user) else {
            break info;
        };
        if asked_at.elapsed() + BUSY_POLL > want.wait {
            let waited = if want.wait.is_zero() {
                String::new()
            } else {
                format!(" (waited {} s)", asked_at.elapsed().as_secs())
            };
            return Err(CliError::new(
                "bench_busy",
                format!(
                    "{driver} is using {} now{waited}. Try again in a few minutes.",
                    link.label
                ),
            ));
        }
        tokio::time::sleep(BUSY_POLL).await;
    };
    if info.provides.is_empty() {
        return Err(unavailable(format!(
            "{}'s bench {} has no board plugged in right now.",
            link.owner_name, link.label
        )));
    }
    Ok(BenchReady {
        link,
        url,
        info,
        client,
        tool,
    })
}

/// This account's Using list from the site; empty from a site that does
/// not have `bench_using` yet.
async fn using(session: &Session) -> Result<Vec<Using>, CliError> {
    match session.client.bench_using().await {
        Ok(rows) => Ok(rows),
        Err(e) if e.code() == "not_on_site" => Ok(Vec::new()),
        Err(e) => Err(session.forget_dead_token(e.into())),
    }
}

/// A Using row as a saved link, or why it cannot be driven now: the site
/// gives the code only while this account is connected and the owner
/// shares.
fn from_row(session: &Session, user: &str, row: &Using) -> Result<Link, CliError> {
    let Some(code) = row.share_code.clone() else {
        let why = if !row.connected {
            format!(
                "You are not connected to {}'s bench {} now. Enter its share code on the Devices page again.",
                row.owner_name, row.label
            )
        } else {
            format!(
                "{} stopped sharing {}. Ask them to share it again.",
                row.owner_name, row.label
            )
        };
        return Err(unavailable(why));
    };
    Ok(Link {
        site: session.client.site_url().to_string(),
        user: user.to_string(),
        bench: row.name.clone(),
        code,
        label: row.label.clone(),
        board: row.board.clone(),
        owner_name: row.owner_name.clone(),
        url: row.url.clone(),
    })
}

/// What the bench is told to flash with.
fn spec(tool: &Tool) -> Result<ToolSpec, CliError> {
    Ok(match tool {
        Tool::Espflash { chip } => ToolSpec {
            program: "espflash".into(),
            chip: chip.clone(),
        },
        Tool::ProbeRs { chip } => ToolSpec {
            program: "probe-rs".into(),
            chip: Some(chip.clone()),
        },
        Tool::Uf2 { family } => {
            return Err(unavailable(format!(
                "A {} board is flashed through its UF2 bootloader, which someone at the board has to start for every run ({}), so it cannot run on a shared bench. Run it on your own board with `ter run --hw`.",
                family.chip, family.bootloader
            )));
        }
    })
}
