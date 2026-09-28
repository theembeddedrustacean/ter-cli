//! The bench venue for `ter run --hw --venue bench`: the board of someone
//! who shares it, reached through their `ter serve --share`.
//!
//! Settled before the build, like the local venue: the site says whether
//! this account may still use the bench and where it is, and the bench
//! itself says whether it answers, which board it has and whether someone
//! else is driving. Any of that failing posts nothing.

use std::path::PathBuf;

use ter_check::CheckFile;
use ter_flash::Tool;
use ter_remote::bench::{BenchClient, BenchError, BenchInfo, RemoteBench, ToolSpec, fits};
use ter_sdk::project::TerToml;
use ter_telemetry::EventKinds;

use crate::bench::{Benches, Link};
use crate::output::CliError;
use crate::session::Session;
use crate::sim::unobservable;

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
    let code = if e.code == "bench_offline" {
        "bench_offline"
    } else {
        "venue_unavailable"
    };
    CliError::new(code, e.message)
}

/// The bench this account connected to last, ready for a run of `ter`'s
/// exercise flashed with `tool`.
pub async fn ready(
    session: &Session,
    user: &str,
    ter: &TerToml,
    tool: &Tool,
) -> Result<BenchReady, CliError> {
    let tool = spec(tool)?;
    let site = session.client.site_url().to_string();
    let mut benches = Benches::load()?;
    let mut link = benches.current(&site, user).cloned().ok_or_else(|| {
        unavailable(
            "You are not connected to a bench. Ask its owner for the share code and run `ter connect <code>`.",
        )
    })?;
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
                "{} is no longer in your list of benches. Run `ter connect <code>` again.",
                link.label
            )),
            _ => session.forget_dead_token(e.into()),
        })?;
    if connected.url.is_some() {
        link.url = connected.url.clone();
    }
    benches.connect(link.clone());
    benches.save()?;
    let url = link.url.clone().ok_or_else(|| {
        unavailable(format!(
            "{}'s bench {} has no address yet: its owner has not started `ter serve --share`.",
            link.owner_name, link.label
        ))
    })?;

    let client = BenchClient::new(&url, &link.code, user).map_err(from_bench)?;
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
        return Err(unavailable(format!(
            "{} is a {}, and {} is for {}.",
            link.label, info.board, ter.exercise, ter.target
        )));
    }
    if let Some(driver) = info.driver.as_deref().filter(|d| *d != user) {
        return Err(unavailable(format!(
            "{driver} is using {} now. Try again in a few minutes.",
            link.label
        )));
    }
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

/// The checks the bench cannot see, as one line for before the build, or
/// `None` when it sees them all.
pub fn unseen_note(file: &CheckFile, provides: &EventKinds) -> Option<String> {
    let unseen: Vec<String> = unobservable(file, provides)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    (!unseen.is_empty()).then(|| {
        format!(
            "This bench shows serial only: {} of {} checks cannot be seen on it ({}).",
            unseen.len(),
            file.checks.len(),
            unseen.join(", ")
        )
    })
}
