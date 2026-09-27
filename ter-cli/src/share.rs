//! `ter serve --share`: this machine's board as a bench others can run on.
//!
//! On start ter registers the bench on the site, turns sharing on and
//! prints the code, then keeps the bench alive with a heartbeat. Runs
//! arrive on the bench socket (`--bind`, `--bench-port`), which a tunnel
//! or the LAN reaches at `--url`; they are flashed and captured on the
//! board here and the recording goes back. Each heartbeat's answer also
//! says whether sharing is on and under which code, so turning it off on
//! the Devices page or with `ter bench unshare` shuts the bench within one
//! interval.
//!
//! The kill switch is a line on standard input: `k` drops whoever is
//! driving and turns sharing off on the site.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ter_flash::{Port, Tool};
use ter_remote::bench::{
    BenchAnswer, BenchError, BenchRun, Board, BoardFuture, Owner, SharedOwner,
};
use ter_remote::heartbeat::{Change, Schedule};
use ter_remote::policy::Limits;
use ter_sdk::Client;
use ter_telemetry::local::Local;
use ter_telemetry::{EventKinds, Kind, Venue};

use crate::bench::{Benches, Served};
use crate::output::CliError;
use crate::session::Session;

/// What `--share` was given.
pub struct ShareArgs {
    pub board: String,
    pub label: Option<String>,
    /// Where others reach the bench socket.
    pub url: Option<String>,
    pub bind: IpAddr,
    pub bench_port: u16,
}

/// The site's heartbeat TTL: no heartbeat for this long and the bench
/// shows offline.
const SITE_TTL: Duration = Duration::from_secs(60);

/// A shared bench, running.
pub struct Sharing {
    pub bench: String,
    pub label: String,
    pub board: String,
    pub code: String,
    pub url: String,
    pub listen: String,
}

/// The board on this machine, as a bench.
struct BenchBoard {
    board: String,
    label: String,
    /// Held for a whole run: page runs and bench runs take turns.
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl Board for BenchBoard {
    fn board(&self) -> &str {
        &self.board
    }

    fn label(&self) -> &str {
        &self.label
    }

    /// What the board on USB shows now; nothing when none is plugged in.
    fn provides(&self) -> EventKinds {
        let tool = Tool::Espflash { chip: None };
        match crate::hw::port(&tool) {
            Ok(port) => Local::provides_on(&tool, port.kind),
            Err(_) => match crate::hw::port(&Tool::ProbeRs {
                chip: String::new(),
            }) {
                Ok(_) => [Kind::Serial].into(),
                Err(_) => EventKinds::new(),
            },
        }
    }

    fn run(&self, run: BenchRun, elf: Vec<u8>) -> BoardFuture {
        let turn = self.turn.clone();
        Box::pin(async move {
            let _turn = turn.lock_owned().await;
            eprintln!(
                "bench: flashing {} ({} bytes), {} ms of serial",
                run.target,
                elf.len(),
                run.timeout_ms
            );
            tokio::task::spawn_blocking(move || flash_and_capture(run, elf))
                .await
                .map_err(|e| {
                    BenchError::new("venue_unavailable", format!("The run stopped: {e}"))
                })?
        })
    }
}

fn tool_of(run: &BenchRun) -> Tool {
    match run.tool.program.as_str() {
        "probe-rs" => Tool::ProbeRs {
            chip: run.tool.chip.clone().unwrap_or_default(),
        },
        _ => Tool::Espflash {
            chip: run.tool.chip.clone(),
        },
    }
}

/// Flash and capture on the board here, in a scratch folder: the run's
/// recording lives on the driver's machine.
fn flash_and_capture(run: BenchRun, elf: Vec<u8>) -> Result<BenchAnswer, BenchError> {
    let unavailable = |m: String| BenchError::new("venue_unavailable", m);
    let tool = tool_of(&run);
    let program = ter_flash::find_program(tool.program()).ok_or_else(|| {
        unavailable(format!(
            "{} is not installed on the bench's machine.",
            tool.program()
        ))
    })?;
    let port: Port = crate::hw::port(&tool).map_err(|e| unavailable(e.message))?;
    let dir = tempfile::tempdir().map_err(|e| unavailable(e.to_string()))?;
    let elf_path = dir.path().join("firmware.elf");
    std::fs::write(&elf_path, &elf).map_err(|e| unavailable(e.to_string()))?;
    let mut local = Local::new(
        tool,
        program,
        port,
        dir.path().to_path_buf(),
        run.timeout_ms,
    );
    local.capture = run.capture.clone();
    let budget = crate::run::board_budget(run.timeout_ms);
    let recorded = local
        .prepare(&elf_path)
        .and_then(|()| local.reset())
        .and_then(|()| local.run(&run.stimuli, budget));
    let read = |name: &str| std::fs::read(dir.path().join(name)).unwrap_or_default();
    let flash_log = String::from_utf8_lossy(&read(ter_telemetry::local::FLASH_LOG)).into_owned();
    match recorded {
        Ok(rec) => Ok(BenchAnswer {
            events: rec.to_jsonl(),
            flash_log,
            serial_log: String::from_utf8_lossy(&read(ter_telemetry::local::SERIAL_LOG))
                .into_owned(),
        }),
        Err(e) => Err(BenchError {
            code: crate::record::Infra::from(e.failure).code().into(),
            message: e.message,
            output: if e.output.is_empty() {
                flash_log
            } else {
                e.output
            },
        }),
    }
}

/// Register, share, open the bench socket and start the heartbeat and the
/// kill switch. Returns once everything runs; the tasks live as long as
/// the process.
pub async fn start(
    session: Session,
    args: ShareArgs,
    turn: Arc<tokio::sync::Mutex<()>>,
) -> Result<Sharing, CliError> {
    let listener = tokio::net::TcpListener::bind((args.bind, args.bench_port))
        .await
        .map_err(|e| {
            CliError::new(
                "io_error",
                format!(
                    "Could not listen on {}:{} for the bench: {e}",
                    args.bind, args.bench_port
                ),
            )
        })?;
    let local = listener
        .local_addr()
        .map_err(|e| CliError::new("io_error", e.to_string()))?;
    let url = match &args.url {
        Some(u) => u.trim_end_matches('/').to_string(),
        None if args.bind.is_unspecified() => {
            return Err(CliError::new(
                "bad_request",
                format!(
                    "The bench listens on every address ({}); say where others reach it with --url.",
                    args.bind
                ),
            ));
        }
        None => format!("http://{local}"),
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CliError::new(
            "bad_request",
            format!("--url must be an http or https address, not {url:?}."),
        ));
    }

    let client = session.client;
    let user = client.ping().await.map_err(CliError::from)?.user.clone();
    let site = client.site_url().to_string();
    let label = args.label.clone().unwrap_or_else(|| args.board.clone());
    let mut benches = Benches::load()?;
    let known = benches
        .served_as(&site, &user, &label)
        .map(|s| s.bench.clone());
    let registered = match client
        .bench_register(&args.board, Some(&label), Some(&url), known.as_deref())
        .await
    {
        // Removed on the site since: register it afresh.
        Err(e) if known.is_some() && e.code() == "not_found" => {
            client
                .bench_register(&args.board, Some(&label), Some(&url), None)
                .await
        }
        other => other,
    }
    .map_err(CliError::from)?;
    let registered_at = Instant::now();
    benches.serve(Served {
        site: site.clone(),
        user,
        bench: registered.bench.clone(),
        board: registered.board.clone(),
        label: registered.label.clone(),
    });
    benches.save()?;
    let shared = client
        .bench_share(&registered.bench)
        .await
        .map_err(CliError::from)?;

    let owner = Owner::new(Limits::default());
    owner
        .lock()
        .expect("owner lock")
        .set_code(Some(shared.share_code.clone()));
    let board = Arc::new(BenchBoard {
        board: registered.board.clone(),
        label: registered.label.clone(),
        turn,
    });
    if board.provides().is_empty() {
        eprintln!(
            "No board is plugged in yet; runs on this bench fail until one is (`ter venues` shows what ter sees)."
        );
    }
    tokio::spawn({
        let owner = owner.clone();
        async move {
            if let Err(e) = ter_remote::bench::serve(listener, board, owner).await {
                eprintln!("bench: the bench socket stopped: {e}");
            }
        }
    });
    let client = Arc::new(client);
    let every = Duration::from_secs(registered.heartbeat_every);
    tokio::spawn(heartbeat(
        client.clone(),
        registered.bench.clone(),
        owner.clone(),
        Schedule::new(every, SITE_TTL, registered_at),
    ));
    kill_switch(client, registered.bench.clone(), owner);

    Ok(Sharing {
        bench: registered.bench,
        label: registered.label,
        board: registered.board,
        code: shared.share_code,
        url,
        listen: format!("http://{local}"),
    })
}

/// Keep the bench alive on the site, and follow sharing turned on or off
/// elsewhere.
async fn heartbeat(client: Arc<Client>, bench: String, owner: SharedOwner, mut schedule: Schedule) {
    let mut wait = schedule.first();
    let mut connected: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(wait).await;
        let busy = owner.lock().expect("owner lock").busy();
        let change = match client.bench_heartbeat(&bench, busy).await {
            Ok(hb) => {
                follow(&owner, hb.share_code.filter(|_| hb.sharing));
                if hb.connected != connected {
                    connected = hb.connected;
                    if connected.is_empty() {
                        eprintln!("Nobody is connected to the bench now.");
                    } else {
                        eprintln!("Connected to the bench now: {}.", connected.join(", "));
                    }
                }
                let (next, change) = schedule.ok(Instant::now());
                wait = next;
                change
            }
            Err(e) if e.code() == "not_found" => {
                owner.lock().expect("owner lock").kill();
                eprintln!(
                    "The bench was removed on the site, so it is no longer shared. The lesson page's Run button still works; restart `ter serve --share` to share again."
                );
                return;
            }
            Err(e) => {
                let (next, change) = schedule.failed(Instant::now());
                wait = next;
                if change.is_some() {
                    eprintln!("Heartbeat failed: {e}");
                }
                change
            }
        };
        match change {
            Some(Change::WentOffline) => eprintln!(
                "The site has not heard from this bench for {} s, so it shows it as offline. Retrying.",
                SITE_TTL.as_secs()
            ),
            Some(Change::BackOnline) => eprintln!("The site sees the bench again."),
            None => {}
        }
    }
}

fn follow(owner: &SharedOwner, code: Option<String>) {
    let mut o = owner.lock().expect("owner lock");
    let was = o.code().map(str::to_string);
    let dropped = o.set_code(code);
    let now = o.code().map(str::to_string);
    if was == now {
        return;
    }
    let dropped = dropped
        .map(|d| format!(" {d} was dropped."))
        .unwrap_or_default();
    match now {
        Some(c) => eprintln!("The bench is shared as {c} now.{dropped}"),
        None => eprintln!(
            "Sharing was turned off on the site.{dropped} `ter bench share` turns it on again."
        ),
    }
}

/// `k` then Enter: drop the driver, stop sharing.
fn kill_switch(client: Arc<Client>, bench: String, owner: SharedOwner) {
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if !line.trim().eq_ignore_ascii_case("k") {
                continue;
            }
            let dropped = owner.lock().expect("owner lock").kill();
            let unshared = handle.block_on(client.bench_unshare(&bench));
            let who = dropped.map_or("Nobody was driving.".to_string(), |d| {
                format!("Dropped {d}.")
            });
            match unshared {
                Ok(_) => eprintln!(
                    "{who} Sharing is off and the code no longer works. `ter bench share` shares again with a new code."
                ),
                Err(e) => eprintln!(
                    "{who} The bench refuses runs, but the site could not be told to stop sharing ({e}); run `ter bench unshare`."
                ),
            }
        }
    });
}

/// Where the bench socket listens unless told: next to the page's port.
pub fn default_bench_port(serve_port: u16) -> u16 {
    if serve_port == 0 {
        0
    } else {
        serve_port.wrapping_add(1)
    }
}
