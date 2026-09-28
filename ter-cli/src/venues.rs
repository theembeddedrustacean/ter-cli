//! `ter venues`: where this machine can run exercises, and what each place
//! can see. Needs no account.

use serde_json::{Value, json};
use ter_flash::{PortKind, Tool};
use ter_telemetry::local::Local;
use ter_telemetry::wokwi::Wokwi;
use ter_telemetry::{EventKinds, Kind};

use crate::hw::{PORT_ENV, UF2_DRIVE_ENV};
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
    // A UF2 board in its bootloader shows a drive, not a port; ter flashes
    // it itself.
    let named_drive = std::env::var(UF2_DRIVE_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .and_then(|p| ter_flash::drive::Drive::at(std::path::Path::new(p.trim())));
    let mut drives = ter_flash::drive::mounted();
    drives.extend(named_drive.filter(|d| !drives.iter().any(|m| m.path == d.path)));
    let uf2_provides: EventKinds = Local::provides_on(
        &Tool::Uf2 {
            family: &ter_flash::uf2::RP2040,
        },
        PortKind::Other,
    );
    let uf2_drives: Vec<Value> = drives
        .iter()
        .map(|d| {
            json!({
                "drive": d.path,
                "board": d.board(),
                "family": d.family().map(|f| f.name),
                "provides": kinds(&uf2_provides),
            })
        })
        .collect();
    let unmounted: Vec<String> = ter_flash::drive::unmounted()
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    let local_ready = (tools.iter().any(|(_, p)| p.is_some())
        && (!boards.is_empty() || port_env.is_some()))
        || !uf2_drives.is_empty();

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
                "uf2_drives": uf2_drives,
                "uf2_unmounted": unmounted,
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
    if boards.is_empty() && uf2_drives.is_empty() {
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
    for d in &drives {
        println!(
            "  {}  UF2 drive of {}  flashed by ter; sees {} once the program sets up USB serial",
            d.path.display(),
            d.board(),
            kinds(&uf2_provides).join(", ")
        );
    }
    for disk in &unmounted {
        println!("  {disk}  UF2 bootloader, not mounted (`udisksctl mount -b {disk}`)");
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
