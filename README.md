# ter

`ter` is the command line tool for [TER Learn](https://learn.theembeddedrustacean.com),
The Embedded Rustacean's course site. It fetches exercises as Cargo
projects, runs them on your board or in simulation, checks the
result and records it on your account.

## Install

```
cargo install ter-cli
```

ter needs Rust 1.98 or newer, so run `rustup update` first.

## Quick start

```
ter login                       pair this machine with your TER Learn account
ter ex fetch <id>               fetch an exercise
cd ~/ter-courses/<course>/<id>
ter install targets wokwi-cli   the exercise's toolchain and the simulator
ter run --sim                   run it in simulation and check it
ter run --hw                    or flash your board and check it
ter hint                        stuck? get the next hint
```

`ter run --sim` runs in [Wokwi](https://wokwi.com) on your own Wokwi
account; a free account works. `ter install wokwi-cli` asks for a CI
token, which you create at <https://wokwi.com/dashboard/ci>.

An exercise that comes with a sim86 board can also run on sim86:
`ter run --sim sim86`. `ter install sim86` sets it up; it needs Python 3.9
or newer and your own sim86 account. `ter ex show` lists the simulators
that can run an exercise. A lesson page that shows the exercise on a sim86
board asks `ter serve` for that run on sim86.

A run on sim86 can be watched as it happens. The lesson page shows the
board live, in sim86's own view of the run, which `ter serve` opens signed
in with your sim86 key (the key goes from ter to your browser and to
sim86, never to the lesson site). In a terminal, `ter run --sim sim86
--live` prints the pin changes and the serial output as they come, until
Ctrl-C or the limit (`--limit <seconds>`, 60 unless given, 600 at most),
then the verdicts.
sim86 charges your account for every minute a board runs, so ter stops a
live run when its time is up, and when the page that started it is closed.

## Commands

```
ter login | logout | whoami      pairing and account
ter ex list | fetch | show | clean | remove
ter run [--hw | --sim [sim86] [--live] | --no-check]
ter status                       your exercises and their last runs
ter hint [--llm]                 a hint from the course, or from your own model
ter llm setup                    choose that model and store your key
ter install <tool>               the toolchain, flashing tools and the simulators
ter new --board <board>          a new project for a board
ter serve                        lets the lesson page's Run button use this machine
ter serve --share | ter connect  share an ESP32-C3 as a bench, or use one
ter venues                       the boards and tools ter can see
ter doctor                       what is missing or wrong here, and the fix
ter self-update                  how to update ter
```

`ter <command> --help` shows the options for each command. Every command
also takes `--json`. For a run that was judged, `ter run --json` includes
what was recorded: `events`, every pin change and serial line with its
time.

## Good to know

- `ter serve` only takes runs from the lesson page on
  learn.theembeddedrustacean.com (or on the site its config names, if you
  set another). `ter serve --dev` also trusts a copy of the site served
  from `http://localhost:8080`.
- Sharing a bench lets whoever has the code flash their own firmware onto
  your board. Firmware can burn eFuses, which cannot be undone, and can
  short pins against whatever is wired to them. Share only with people you
  trust. Only the XIAO ESP32-C3 can be shared for now.
- `ter login` approves the machine on the site's `/cli` page. Your
  password never reaches `ter`, and the token is kept in the system
  keychain.
- Simulation and `ter hint --llm` run on your own accounts and keys.
  TER Learn never holds them.
- A failed flash, an unplugged board or a simulation that cannot
  start is recorded as not run, never as failed.
- Each checked run is saved in the exercise's `.runs/` folder.

- `ter ex fetch <id>` puts exercises in `~/ter-courses/<course>/<id>`.
  To keep them elsewhere, set `courses_root = "<dir>"` in the config
  file or the `TER_COURSES_ROOT` environment variable, or pass
  `--root <dir>` to a command. `ter ex fetch <id> <dir>` puts one
  exercise in a folder of your choice.
- A board run reads serial only, so it can complete an exercise only
  when every check is a serial check with no button presses. Pin checks
  (a blink rate, a pulse width) are graded in simulation: your board
  still shows the LED, but only `ter run --sim` completes the exercise.
  `ter ex show <id>` says which run can pass an exercise.
- `ter new --list-boards` lists the boards. The XIAO boards are generated
  by xiao-generate (`ter install xiao-generate`). The Nucleo-F401RE is
  ter's own template: a blinky that flashes over the board's ST-LINK with
  probe-rs (`ter install targets probe-rs` in the project folder).
- A fetched exercise and a `ter new` project get a `.gitignore` that
  covers `.ter/`, `.runs/` and `target/`, so they are ready for your own
  git repository.

- Something not working? Run `ter doctor` in the exercise folder. It
  checks ter, your account, Rust, the tools, your board and (on Linux)
  whether you may use its port, and prints the fix for each problem. It
  only looks: nothing is installed, built or flashed.

Settings live in `~/.config/ter/config.toml`, and every key is optional.

## Development

```
scripts/check.sh      fmt, clippy, tests and the dependency rule
```

`ter-check`, `ter-telemetry` and `ter-flash` must never reach the
network; `scripts/check-deps.sh` enforces it.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
