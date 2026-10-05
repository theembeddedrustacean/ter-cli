# ter

`ter` is the command line tool for [TER Learn](https://learn.theembeddedrustacean.com),
The Embedded Rustacean's course site. It fetches exercises as Cargo
projects, runs them on your board or in simulation, checks the
result and records it on your account.

## Install

```
cargo install ter-cli
```

On Debian and Ubuntu, install `pkg-config libudev-dev` first.

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

## Commands

```
ter login | logout | whoami      pairing and account
ter ex list | fetch | clean | remove
ter run [--hw | --sim | --no-check]
ter status                       your exercises and their last runs
ter hint [--llm]                 a hint from the course, or from your own model
ter llm setup                    choose that model and store your key
ter install <tool>               the toolchain, flashing tools and the simulator
ter new --board <board>          a new project for a board
ter serve                        lets the lesson page's Run button use this machine
ter serve --share | ter connect  share a board as a bench, or use one
ter venues                       the boards and tools ter can see
ter self-update                  how to update ter
```

`ter <command> --help` shows the options for each command. Every command
also takes `--json`.

## Good to know

- `ter login` approves the machine on the site's `/cli` page. Your
  password never reaches `ter`, and the token is kept in the system
  keychain.
- Simulation and `ter hint --llm` run on your own accounts and keys.
  TER Learn never holds them.
- A failed flash, an unplugged board or a simulation that cannot
  start is recorded as not run, never as failed.
- Each checked run is saved in the exercise's `.runs/` folder.

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
