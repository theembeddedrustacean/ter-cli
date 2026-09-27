//! `ter hint`: the site's next hint for the last run of an exercise.

use std::path::{Path, PathBuf};

use serde::Serialize;
use ter_sdk::run::{HintFile, LastRun};

use crate::output::{CliError, print_json};
use crate::run::exercise_dir;
use crate::session::Session;

/// Larger files are not sent with a hint request.
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Serialize)]
struct HintOut<'a> {
    exercise: &'a str,
    run: &'a str,
    attempt: u32,
    number: u32,
    text: &'a str,
    exhausted: bool,
    /// Hints taken on this run so far; the next run reports it.
    hints_used: u32,
}

pub async fn hint(dir: Option<PathBuf>, json: bool) -> Result<(), CliError> {
    let dir = exercise_dir(dir)?;
    let mut last = LastRun::load(&dir)?.ok_or_else(|| {
        CliError::new(
            "no_run",
            "No run has been posted from this folder yet. `ter run` posts one; hints are for a run.",
        )
    })?;
    let session = Session::open()?;
    let client = &session.client;
    let ping = client
        .ping()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    if last.user != ping.user || last.site != client.site_url() {
        return Err(CliError::new(
            "no_run",
            format!(
                "The last run here was posted by {} on {}, not by this account. `ter run` posts one of yours.",
                last.user, last.site
            ),
        ));
    }

    let files = source_files(&dir);
    let answer = client
        .hint(&last.answer.name, &files)
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    last.hints.record(&answer);
    last.last_hint = Some(answer.clone());
    last.save(&dir)?;

    if json {
        print_json(&HintOut {
            exercise: &last.record.exercise_id,
            run: &last.answer.name,
            attempt: last.answer.attempt,
            number: answer.number,
            text: &answer.text,
            exhausted: answer.exhausted,
            hints_used: last.hints.used,
        });
        return Ok(());
    }
    if answer.number == 0 {
        println!("No hint for attempt {}.", last.answer.attempt);
    } else {
        println!(
            "Hint {} for attempt {}:",
            answer.number, last.answer.attempt
        );
    }
    println!();
    println!("{}", answer.text.trim_end());
    Ok(())
}

/// The learner's `Cargo.toml` and everything under `src/`, as text.
pub(crate) fn source_files(dir: &Path) -> Vec<HintFile> {
    let mut paths = vec![PathBuf::from("Cargo.toml")];
    collect(dir, Path::new("src"), &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter_map(|rel| {
            let path = dir.join(&rel);
            let meta = std::fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            let path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Some(HintFile { path, content })
        })
        .collect()
}

fn collect(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let rel = rel.join(entry.file_name());
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect(root, &rel, out),
            Ok(t) if t.is_file() => out.push(rel),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_the_manifest_and_the_source_tree_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for (path, body) in [
            ("Cargo.toml", "[package]\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/bin/extra.rs", "fn main() {}\n"),
            ("ter.toml", "x"),
            ("target/debug/x", "x"),
            ("check.yaml", "checks: []\n"),
        ] {
            std::fs::create_dir_all(d.join(path).parent().unwrap()).unwrap();
            std::fs::write(d.join(path), body).unwrap();
        }
        std::fs::write(d.join("src/blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let files = source_files(d);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Cargo.toml", "src/bin/extra.rs", "src/main.rs"]);
    }
}
