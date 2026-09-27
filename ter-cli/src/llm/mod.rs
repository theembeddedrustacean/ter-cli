//! Hints from a model the learner chooses, on the learner's own account
//! with that provider (`ter hint --llm`), and `ter llm` to set it up.
//!
//! The key is the learner's and stays on this machine: in the keychain or
//! a mode 600 file, sent only to the provider, never printed, never to
//! TER Learn. With the learner's consent, the exchange (what was sent and
//! the answer) is shared with TER Learn afterwards.

pub mod prompt;
pub mod provider;

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use ter_sdk::run::{HintExchange, LastRun};
use ter_telemetry::rfc3339_utc;

use crate::config::Config;
use crate::hint::source_files;
use crate::output::{CliError, print_json};
use crate::run::exercise_dir;
use crate::session::Session;
use crate::token_store::TokenStore;
use provider::{Preset, Provider};

/// Takes the place of the stored key, for scripts and tests.
pub const KEY_ENV: &str = "TER_LLM_KEY";
/// Where exchanges are kept, under the exercise's `.ter`.
const EXCHANGES: &str = "llm";

/// `ter llm setup`'s flags.
pub struct Setup {
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub key_stdin: bool,
    /// `Some(true)` for --share, `Some(false)` for --no-share.
    pub share: Option<bool>,
}

#[derive(Serialize)]
struct SetupOut<'a> {
    provider: &'a str,
    model: &'a str,
    base_url: &'a str,
    /// Where the key is kept: `keychain`, `file`, `TER_LLM_KEY`, or `none`.
    key: &'a str,
    share: Option<bool>,
}

pub fn setup(args: Setup, json: bool) -> Result<(), CliError> {
    let preset = provider::preset(&args.provider)?;
    let model = args
        .model
        .clone()
        .or(preset.default_model.map(str::to_string))
        .ok_or_else(|| {
            CliError::new(
                "llm_not_configured",
                format!("{} needs a model: --model <name>.", preset.name),
            )
        })?;
    let base_url = args
        .base_url
        .clone()
        .or(preset.base_url.map(str::to_string))
        .ok_or_else(|| {
            CliError::new(
                "llm_not_configured",
                format!(
                    "{} needs the provider's address: --base-url https://.../v1.",
                    preset.name
                ),
            )
        })?;
    if !base_url.starts_with("https://") && !base_url.starts_with("http://") {
        return Err(CliError::new(
            "llm_not_configured",
            format!("--base-url must start with https:// or http://, not {base_url:?}."),
        ));
    }

    let store = TokenStore::open_llm(preset.name)?;
    let key_source = if args.key_stdin {
        let key = read_line(std::io::stdin().lock())?;
        save_key(&store, &key)?
    } else if let Some((_, location)) = store.load()? {
        location.source()
    } else if env_key().is_some() {
        KEY_ENV
    } else if !preset.needs_key {
        "none"
    } else if std::io::stdin().is_terminal() && !json {
        println!(
            "Hints from {} run on your own account there. Create a key at {} and paste it here. It stays on this machine; TER Learn never sees it.",
            preset.name,
            preset.key_page.unwrap_or("your provider's site")
        );
        let key = rpassword::prompt_password(format!("{} key: ", preset.name))
            .map_err(|e| CliError::new("io_error", format!("Could not read the key: {e}")))?;
        save_key(&store, &key)?
    } else {
        return Err(CliError::new(
            "llm_not_configured",
            format!(
                "No {} key yet. Create one at {} and pass it on standard input: `ter llm setup --provider {} --key-stdin`.",
                preset.name,
                preset.key_page.unwrap_or("your provider's site"),
                preset.name
            ),
        ));
    };

    let keep_url = args.base_url.is_some().then(|| base_url.clone());
    let mut changes: Vec<(&str, Option<toml::Value>)> = vec![
        ("llm_provider", Some(preset.name.into())),
        ("llm_model", Some(model.clone().into())),
        ("llm_base_url", keep_url.map(Into::into)),
    ];
    if let Some(share) = args.share {
        changes.push(("llm_share", Some(share.into())));
    }
    Config::update(&changes)?;
    let share = Config::load()?.llm_share;

    if json {
        print_json(&SetupOut {
            provider: preset.name,
            model: &model,
            base_url: &base_url,
            key: key_source,
            share,
        });
    } else {
        println!(
            "`ter hint --llm` asks {} ({model}); key: {key_source}.",
            preset.name
        );
        println!("{}", share_line(share));
    }
    Ok(())
}

#[derive(Serialize)]
struct StatusOut<'a> {
    provider: Option<&'a str>,
    model: Option<&'a str>,
    base_url: Option<&'a str>,
    key: &'a str,
    share: Option<bool>,
}

pub fn status(json: bool) -> Result<(), CliError> {
    let config = Config::load()?;
    let Some(name) = config.llm_provider.as_deref() else {
        if json {
            print_json(&StatusOut {
                provider: None,
                model: None,
                base_url: None,
                key: "none",
                share: config.llm_share,
            });
        } else {
            println!(
                "No hint model set up. `ter llm setup --provider P` with one of: {}.",
                provider::names()
            );
        }
        return Ok(());
    };
    let preset = provider::preset(name)?;
    let key = key_source(preset)?;
    let model = config.llm_model.as_deref().or(preset.default_model);
    let base_url = config.llm_base_url.as_deref().or(preset.base_url);
    if json {
        print_json(&StatusOut {
            provider: Some(preset.name),
            model,
            base_url,
            key,
            share: config.llm_share,
        });
    } else {
        println!("provider  {}", preset.name);
        println!("model     {}", model.unwrap_or("(not set)"));
        println!("address   {}", base_url.unwrap_or("(not set)"));
        println!("key       {key}");
        println!("{}", share_line(config.llm_share));
    }
    Ok(())
}

/// Remove the stored key for `provider` (the configured one by default).
pub fn forget(provider: Option<String>, json: bool) -> Result<(), CliError> {
    let name = match provider.or(Config::load()?.llm_provider) {
        Some(n) => n,
        None => {
            return Err(CliError::new(
                "llm_not_configured",
                "No hint model set up; name one: `ter llm forget --provider P`.",
            ));
        }
    };
    let preset = provider::preset(&name)?;
    let removed = TokenStore::open_llm(preset.name)?.clear()?;
    if json {
        print_json(&serde_json::json!({
            "provider": preset.name,
            "removed": removed.iter().map(|l| l.source()).collect::<Vec<_>>(),
        }));
    } else if removed.is_empty() {
        println!("No {} key was stored on this machine.", preset.name);
    } else {
        println!("Removed your {} key from this machine.", preset.name);
    }
    Ok(())
}

fn share_line(share: Option<bool>) -> &'static str {
    match share {
        Some(true) => "Hints from the model are shared with TER Learn (--no-share to stop).",
        Some(false) => {
            "Hints from the model stay on this machine (--share to share them with TER Learn)."
        }
        None => "You are asked once whether to share hints from the model with TER Learn.",
    }
}

fn env_key() -> Option<String> {
    std::env::var(KEY_ENV)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn key_source(preset: &Preset) -> Result<&'static str, CliError> {
    if env_key().is_some() {
        return Ok(KEY_ENV);
    }
    Ok(match TokenStore::open_llm(preset.name)?.load()? {
        Some((_, location)) => location.source(),
        None => "none",
    })
}

fn save_key(store: &TokenStore, key: &str) -> Result<&'static str, CliError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(CliError::new("llm_not_configured", "The key was empty."));
    }
    let saved = store.save(key)?;
    Ok(saved.location.source())
}

fn read_line(input: impl BufRead) -> Result<String, CliError> {
    let line = input
        .lines()
        .next()
        .transpose()
        .map_err(|e| CliError::new("io_error", format!("Could not read the key: {e}")))?
        .unwrap_or_default();
    Ok(line.trim().to_string())
}

/// The configured provider, ready to call.
fn configured() -> Result<(Provider, Option<bool>), CliError> {
    let config = Config::load()?;
    let name = config.llm_provider.as_deref().ok_or_else(|| {
        CliError::new(
            "llm_not_configured",
            format!(
                "No hint model set up. `ter llm setup --provider P`, with P one of {}, on your own account there.",
                provider::names()
            ),
        )
    })?;
    let preset = provider::preset(name)?;
    let model = config
        .llm_model
        .clone()
        .or(preset.default_model.map(str::to_string))
        .ok_or_else(|| {
            CliError::new(
                "llm_not_configured",
                format!("No model set for {name}: `ter llm setup --provider {name} --model M`."),
            )
        })?;
    let base_url = config
        .llm_base_url
        .clone()
        .or(preset.base_url.map(str::to_string))
        .ok_or_else(|| {
            CliError::new(
                "llm_not_configured",
                format!(
                    "No address set for {name}: `ter llm setup --provider {name} --base-url URL`."
                ),
            )
        })?;
    let key = match env_key() {
        Some(k) => Some(k),
        None => TokenStore::open_llm(preset.name)?.load()?.map(|(k, _)| k),
    };
    if key.is_none() && preset.needs_key {
        return Err(CliError::new(
            "llm_not_configured",
            format!("No {name} key on this machine. `ter llm setup --provider {name}` stores one."),
        ));
    }
    Ok((
        Provider {
            preset,
            model,
            base_url,
            key,
        },
        config.llm_share,
    ))
}

#[derive(Serialize)]
struct HintOut<'a> {
    exercise: &'a str,
    run: &'a str,
    attempt: u32,
    provider: &'a str,
    model: &'a str,
    text: &'a str,
    /// The exchange, kept under the exercise's `.ter/llm/`.
    saved: &'a Path,
    /// `posted`, `not_on_site`, `declined`, `not_asked` or `failed`.
    shared: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    share_error: Option<&'a str>,
}

#[derive(Serialize)]
struct DryRunOut<'a> {
    exercise: &'a str,
    run: &'a str,
    system: &'a str,
    prompt: &'a str,
    left_out: &'a [String],
}

/// `ter hint --llm`: one hint for the last run from the learner's model.
pub async fn hint(dir: Option<PathBuf>, dry_run: bool, json: bool) -> Result<(), CliError> {
    let dir = exercise_dir(dir)?;
    let last = LastRun::load(&dir)?.ok_or_else(|| {
        CliError::new(
            "no_run",
            "No run has been posted from this folder yet. `ter run` posts one; hints are for a run.",
        )
    })?;
    // The provider is checked before anything else is asked of anyone.
    let configured = if dry_run { None } else { Some(configured()?) };

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
    let exercise = client
        .exercise(&last.record.exercise_id)
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;

    let checks = std::fs::read_to_string(dir.join(&last.recording).join("check.toml")).ok();
    let files = source_files(&dir);
    let p = prompt::assemble(&prompt::Context {
        exercise_id: &last.record.exercise_id,
        title: &exercise.title,
        instructions_md: &exercise.instructions_md,
        record: &last.record,
        attempt: last.answer.attempt,
        checks: checks.as_deref(),
        site_hint: last.last_hint.as_ref(),
        files: &files,
    });

    let Some((provider, share)) = configured else {
        if json {
            print_json(&DryRunOut {
                exercise: &last.record.exercise_id,
                run: &last.answer.name,
                system: &p.system,
                prompt: &p.user,
                left_out: &p.left_out,
            });
        } else {
            println!("Nothing is sent with --dry-run. This is what `ter hint --llm` would send:\n");
            println!("--- instructions ---\n{}\n", p.system);
            println!("--- message ---\n{}", p.user);
        }
        return Ok(());
    };
    if last.record.build_status == "passed" && last.record.check_status == "passed" {
        let text = "The last run passed; there is nothing to hint at.";
        if json {
            print_json(
                &serde_json::json!({"exercise": last.record.exercise_id, "run": last.answer.name, "text": text}),
            );
        } else {
            println!("{text}");
        }
        return Ok(());
    }

    if !json {
        eprintln!("Asking {} ({})...", provider.preset.name, provider.model);
    }
    let answer = provider.ask(&p.system, &p.user).await?;
    let exchange = HintExchange {
        run: last.answer.name.clone(),
        exercise_id: last.record.exercise_id.clone(),
        provider: provider.preset.name.to_string(),
        model: provider.model.clone(),
        system: p.system,
        prompt: provider.redact(&p.user),
        answer,
        asked_at: rfc3339_utc(std::time::SystemTime::now()),
        cli_version: client.cli_version().to_string(),
    };
    let saved = save_exchange(&dir, &exchange)?;

    if !json {
        println!(
            "Hint from {} ({}) for attempt {}:\n",
            exchange.provider, exchange.model, last.answer.attempt
        );
        println!("{}", exchange.answer.trim_end());
    }

    let share = match share {
        Some(s) => Some(s),
        None if !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            let s = ask_consent()?;
            Config::update(&[("llm_share", Some(s.into()))])?;
            Some(s)
        }
        None => None,
    };
    let (shared, share_error) = match share {
        None => ("not_asked", None),
        Some(false) => ("declined", None),
        Some(true) => match client.hint_exchange(&exchange).await {
            Ok(_) => ("posted", None),
            Err(e) if e.code() == "not_on_site" => ("not_on_site", None),
            Err(e) => ("failed", Some(session.forget_dead_token(e.into()).message)),
        },
    };
    let rel = saved.strip_prefix(&dir).unwrap_or(&saved);
    if json {
        print_json(&HintOut {
            exercise: &exchange.exercise_id,
            run: &exchange.run,
            attempt: last.answer.attempt,
            provider: &exchange.provider,
            model: &exchange.model,
            text: &exchange.answer,
            saved: rel,
            shared,
            share_error: share_error.as_deref(),
        });
    } else {
        match shared {
            "posted" => eprintln!("\nShared with TER Learn."),
            "not_on_site" => eprintln!(
                "\nTER Learn does not take shared hints yet; this one is kept in {}.",
                rel.display()
            ),
            "failed" => eprintln!(
                "\nCould not share it with TER Learn ({}); it is kept in {}.",
                share_error.as_deref().unwrap_or("unknown"),
                rel.display()
            ),
            _ => {}
        }
    }
    Ok(())
}

fn ask_consent() -> Result<bool, CliError> {
    print!(
        "\nShare hints like this one with TER Learn, to improve the course's own hints? \
It sends what was sent to the model (the lesson, the run, your code) and its answer, never your key. \
Asked once; `ter llm setup --share` or `--no-share` changes it. [y/N] "
    );
    std::io::stdout().flush().ok();
    let answer = read_line(std::io::stdin().lock())?;
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `.ter/llm/<run>-<n>.json`, the next free `n`.
fn save_exchange(dir: &Path, exchange: &HintExchange) -> Result<PathBuf, CliError> {
    let folder = dir.join(ter_sdk::project::STATE_DIR).join(EXCHANGES);
    let fail = |e: std::io::Error| {
        CliError::new(
            "io_error",
            format!("Could not write in {}: {e}", folder.display()),
        )
    };
    std::fs::create_dir_all(&folder).map_err(fail)?;
    let body = serde_json::to_string_pretty(exchange).expect("exchange serialises");
    for n in 1.. {
        let path = folder.join(format!("{}-{n}.json", exchange.run));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(body.as_bytes()).map_err(fail)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fail(e)),
        }
    }
    unreachable!()
}
