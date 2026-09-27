//! `ter venues`: where this machine can run exercises, and what each place
//! can see. Needs no account.

use serde_json::{Value, json};
use ter_flash::{PortKind, Tool};
use ter_telemetry::local::Local;
use ter_telemetry::wokwi::Wokwi;
use ter_telemetry::{EventKinds, Kind};

use crate::hw::PORT_ENV;
use crate::output::{CliError, print_json};
use crate::sim;

fn kinds(k: &EventKinds) -> Vec<String> {
    k.iter().map(Kind::to_string).collect()
}

pub fn run(json: bool) -> Result<(), CliError> {
    let tools: Vec<(&str, Option<String>)> = ["espflash", "probe-rs"]
        .into_iter()
        .map(|t| {
            (
                t,
                ter_flash::find_program(t).map(|p| p.display().to_string()),
            )
        })
        .collect();
    let boards: Vec<Value> = ter_flash::port::list()
        .into_iter()
        .filter(|p| p.kind != PortKind::Other)
        .map(|p| {
            // What a bare board on this port shows, flashed by its tool.
            let tool = if p.kind == PortKind::Probe {
                Tool::ProbeRs {
                    chip: String::new(),
                }
            } else {
                Tool::Espflash { chip: None }
            };
            json!({
                "port": p.path,
                "kind": p.kind.to_string(),
                "usb": p.usb,
                "product": p.product,
                "provides": kinds(&Local::provides_on(&tool, p.kind)),
            })
        })
        .collect();
    let port_env = std::env::var(PORT_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty());
    let local_ready =
        tools.iter().any(|(_, p)| p.is_some()) && (!boards.is_empty() || port_env.is_some());

    let wokwi_cli = sim::find_cli().map(|p| p.display().to_string());
    let wokwi_token = matches!(sim::token(), Ok(Some(_)));
    let wokwi_provides: EventKinds = Wokwi::PROVIDES.into();

    let answer = json!({
        "venues": [
            {
                "name": "local",
                "mode": "hardware",
                "ready": local_ready,
                "boards": boards,
                "port_env": port_env,
                "tools": tools.iter().map(|(t, p)| (t.to_string(), json!(p))).collect::<serde_json::Map<_, _>>(),
            },
            {
                "name": "wokwi",
                "mode": "simulation",
                "ready": wokwi_cli.is_some() && wokwi_token,
                "provides": kinds(&wokwi_provides),
                "wokwi_cli": wokwi_cli,
                "token": wokwi_token,
            }
        ],
        "instruments": [],
    });
    if json {
        print_json(&answer);
        return Ok(());
    }

    println!("local (hardware): your board on USB");
    if boards.is_empty() {
        println!("  no board found on USB");
    }
    for b in &boards {
        println!(
            "  {}  {}{}  sees {}",
            b["port"].as_str().unwrap_or_default(),
            b["kind"].as_str().unwrap_or_default(),
            b["usb"]
                .as_str()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
            b["provides"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
    }
    if let Some(p) = &port_env {
        println!("  {PORT_ENV}={p}");
    }
    for (t, p) in &tools {
        match p {
            Some(p) => println!("  {t}: {p}"),
            None => println!("  {t}: not installed"),
        }
    }
    println!("  pins, power and buttons: not seen without the telemetry board");
    println!();
    println!(
        "wokwi (simulation): sees {}",
        kinds(&wokwi_provides).join(", ")
    );
    match &wokwi_cli {
        Some(p) => println!("  wokwi-cli: {p}"),
        None => println!("  wokwi-cli: not installed (`ter install wokwi-cli`)"),
    }
    println!(
        "  token: {}",
        if wokwi_token {
            "set"
        } else {
            "none (`ter install wokwi-cli` stores one)"
        }
    );
    println!();
    println!("instruments: none");
    Ok(())
}
