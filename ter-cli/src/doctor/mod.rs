//! `ter doctor`: what is missing or wrong on this machine, and the command
//! that fixes each thing.
//!
//! It only looks. It never builds, flashes, opens a serial port, talks to
//! a chip, spends Wokwi minutes or sim86 credits, or changes the machine;
//! the site is asked one `ping`, and sim86, where it is installed, for the
//! account's plan and credits (neither with `--offline`). In an exercise or project folder it
//! judges for that folder's run; elsewhere only what every run needs can
//! fail.

mod gather;
mod judge;

use std::path::PathBuf;

use serde_json::json;

use crate::output::{CliError, print_json};
use judge::{Report, Status};

pub struct Args {
    /// The folder to judge for; the current one by default.
    pub dir: Option<PathBuf>,
    /// Do not ask the site.
    pub offline: bool,
    /// A curriculum checkout: also compare the tools with its versions.
    pub dev: Option<PathBuf>,
}

pub async fn run(args: Args, json: bool) -> Result<(), CliError> {
    let report = judge::judge(&gather::gather(&args).await?);
    if json {
        print_json(&report);
    } else {
        print!("{}", text(&report));
    }
    if report.ok {
        return Ok(());
    }
    let problems = report.count(Status::Fail);
    let mut err = CliError::new(
        "not_ready",
        format!(
            "{problems} {} on this machine. Each has its fix above.",
            if problems == 1 { "problem" } else { "problems" }
        ),
    );
    if json {
        // The report is the answer on stdout; the error goes beside it.
        err.message = format!(
            "{problems} {} on this machine.",
            if problems == 1 { "problem" } else { "problems" }
        );
        eprintln!(
            "{}",
            json!({"error": {"code": err.code, "message": err.message}})
        );
        err.in_json_answer = true;
    }
    Err(err)
}

/// The report for people: a line per check under its group, the fix
/// under each problem, then what `ter install` can do and the count.
fn text(report: &Report) -> String {
    let context = &report.context;
    let mut out = match (context.kind, &context.mode, &context.venue) {
        ("exercise", Some(mode), Some(venue)) => format!(
            "{}: an exercise, run in {mode} mode on {venue}\n",
            context.dir.display()
        ),
        ("none", ..) => format!(
            "{}: not an exercise or project folder, so only what every run needs is judged\n",
            context.dir.display()
        ),
        (kind, ..) => format!("{}: {kind} folder\n", context.dir.display()),
    };
    let mut group = "";
    for check in &report.checks {
        if check.group != group {
            group = check.group;
            out.push_str(&format!("\n{group}\n"));
        }
        let mark = match check.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
            Status::Skip => "-",
        };
        out.push_str(&format!("  {mark:<4}  {:<18}  {}\n", check.id, check.found));
        let under = " ".repeat(28);
        if let Some(why) = &check.why {
            out.push_str(&format!("{under}{why}\n"));
        }
        if let Some(fix) = &check.fix {
            out.push_str(&format!("{under}fix: {fix}\n"));
        }
    }
    out.push('\n');
    if !report.install.is_empty() {
        out.push_str(&format!(
            "Install what is missing: ter install {}\n",
            report.install.join(" ")
        ));
    }
    let count = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let (problems, warnings) = (report.count(Status::Fail), report.count(Status::Warn));
    out.push_str(&match (problems, warnings) {
        (0, 0) => "Nothing wrong found.\n".to_string(),
        (0, w) => format!("No problems, {}.\n", count(w, "warning")),
        (p, 0) => format!("{}.\n", count(p, "problem")),
        (p, w) => format!("{}, {}.\n", count(p, "problem"), count(w, "warning")),
    });
    out
}
