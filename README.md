# ter

`ter` is the command line tool for [TER Learn](https://learn.theembeddedrustacean.com),
The Embedded Rustacean's course site. It pairs your machine with your TER
Learn account, fetches exercises as small Cargo projects, builds and runs
them on your board or in a simulator, checks what the program did, and
records the attempt on the site.

This is an early version: pairing (`ter login`, `ter logout`,
`ter whoami`), exercises (`ter ex`), checked runs on your own board
(`ter run --hw`) and in the Wokwi simulator (`ter run --sim`), build-only
runs (`ter run --no-check`), `ter venues`, `ter status`, `ter hint`,
`ter telemetry check` and `ter self-update` work today.

## Install

```
cargo install --git <repository URL> ter-cli
```

On Debian and Ubuntu, install `pkg-config libudev-dev` first.

## Use

```
ter login             pair this machine with your TER Learn account
ter logout            remove this machine's device token
ter whoami            the account, courses and versions
ter ex list           the exercises in your courses (--course C for one)
ter ex fetch <id>     fetch an exercise as a Cargo project
ter ex clean          delete build output, keep the source
ter ex remove <id>    delete an exercise's folder
ter run --hw          build, flash your board, check, and record the run on the site
ter run --sim         build, run in Wokwi, check, and record the run on the site
ter run --no-check    build the exercise and record the attempt on the site
ter venues            where this machine can run exercises, and what each sees
ter telemetry check   judge a recorded run against a check.yaml, offline
ter install wokwi-cli install Wokwi's simulator and store your Wokwi token
ter status            your exercises and their last runs (--disk: disk use)
ter hint              the next hint for the last run
ter self-update       how to update ter
```

Every command takes `--json`. On failure `ter` exits non-zero and prints a
stable error code (`token_invalid`, `not_enrolled`, `outdated`, ...).

`ter login` prints a short code and a link to the site's `/cli` page (and
opens it when there is a browser; `--no-browser` skips that). Approve the
code there while signed in, and `ter` receives a device token. No password
ever reaches `ter`.

The token is kept in the system keychain: Keychain on macOS, Credential
Manager on Windows, the Secret Service (GNOME Keyring, KWallet) on Linux.
Where there is none, as on a headless machine or WSL, it goes to
`~/.config/ter/token`, readable only by you, and `ter login` says so.
`TER_KEYCHAIN=off` always uses the file. `ter logout` removes the token
from both; to cut the device off on the site as well, revoke it on your
Devices page. When the site reports the token revoked or expired, `ter`
removes it and asks you to `ter login` again.

`TER_TOKEN`, when set, is used instead of the stored token.

### Exercises

`ter ex fetch <id>` puts an exercise in `<courses-root>/<course>/<id>/`
(`~/ter-courses` by default), or in the folder you name after the id. It
is an ordinary Cargo project: `cargo run` in it builds the program,
flashes your board and shows its serial output. Next to the project,
`ter.toml` records the exercise, its course and target, and the `mode`
(`hardware` or `simulation`) a bare `ter run` uses; you may change `mode`.

When `ter` builds an exercise, every exercise in a course shares one build
cache, `<course>/.target` (and `<course>/.embuild` for ESP-IDF tools), so
the HAL compiles once per course. `ter ex clean` in an exercise folder, or
`ter ex clean <id>`, drops that exercise's build output and run
recordings; `--course C` also drops the course's cache, `--all` every
course's. `ter ex remove <id>` deletes the folder; if you have changed the
source since fetching, it refuses unless you add `--force`.

### Runs and hints

`ter run` in an exercise folder builds it (into the course's shared
cache), runs it, checks it against the exercise's `check.yaml` and posts
the run to TER Learn. `--sim` or `--hw` picks the mode for one run instead
of `ter.toml`'s. A failed build is posted with the end of the compiler
output. `ter run --no-check` builds only and posts the attempt as built
but not checked. An exercise with no `check.yaml` builds only in either
mode: "This exercise has no automatic check yet. Run it with cargo run."

On hardware the program runs on your board, plugged in over USB. `ter`
flashes it with the tool the exercise's `cargo run` uses (espflash or
probe-rs; install it first, e.g. `cargo install espflash --locked`),
then listens to the board's serial port for the check's time. On an ESP
board `ter` resets the chip itself once the port is open, so it sees the
program from its first line, and times in `check.yaml` count from that
reset. If one board is plugged in `ter` finds it; with several, set
`TER_PORT` to the one to use (on Linux you need to be in the `dialout`
group). `ter venues` lists the boards and tools it sees.

A bare board shows serial output only. Checks on pins, and checks that
press a button, cannot be seen there: `ter` names them as unseen, and if
every check it could see passed, the run is a partial pass (`1 of 2
checks seen on this setup, all passed. Full check: ter run --sim`),
posted with the counts so the site does not count it towards mastery. If
no check can be seen, the board still runs and its output is shown, and
the run is posted as not run. A flash that fails, or a board unplugged
during the run, is posted as not run, never as failed.

In simulation the program runs in [Wokwi](https://wokwi.com), on your own
Wokwi account: `ter install wokwi-cli` installs `wokwi-cli` and stores
your Wokwi CI token (from https://wokwi.com/dashboard/ci) the way it
stores the TER token; `WOKWI_CLI_TOKEN`, when set, is used instead. The
exercise's circuit (`diagram.json`) is copied into the run folder with a
logic analyzer added on the pins the checks watch, and a scenario presses
the buttons the check names at the times it gives. Your `diagram.json` is
never changed. If Wokwi cannot be reached, refuses the token or your
account is out of simulation time, the run is posted as not run, never as
failed.

Every checked run is a recording in `.runs/<n>/`: the build log; on a
board, what the flashing tool printed (`flash.log`) and the raw serial
bytes (`serial.log`); in Wokwi, the run's circuit and scenario and what
Wokwi captured (`wokwi.vcd`, `serial.log`); the event stream (`events.jsonl`), the `check.yaml` it was judged against
and the verdicts (`check.toml`). `ter telemetry check .runs/<n> --check
check.yaml` judges a recording again with no simulator and no account,
and prints the same `check.toml`; after a change to `check.yaml` it shows
what the new check makes of the old run.

`ter hint` asks the site for the next hint on the last run posted from
the folder; the next `ter run` reports how many you took. `ter status` in
an exercise folder shows its last run, attempt number and how the concepts
it practises moved; elsewhere it lists every exercise in your courses.
`ter status --disk` shows what each course takes on disk, without asking
the site. What `ter` remembers about the last run is in `.ter/` in the
exercise; `ter ex clean` keeps it.

For curriculum authors, `ter ex fetch <id> --dev <checkout>` builds the
exercise from a local curriculum checkout (with its
`tools/exercise_sync.py`) instead of the site, and needs no account.

## Configuration

`~/.config/ter/config.toml` on Linux (the platform config directory
elsewhere); every key is optional:

```toml
site_url = "https://learn.theembeddedrustacean.com"
courses_root = "/home/you/ter-courses"
serve_port = 7357
```

`TER_CONFIG_DIR` and `TER_SITE_URL` override the directory and the site.

## Development

The workspace is split by what each crate may reach:

| crate | does | network |
|---|---|---|
| `ter-cli` | the `ter` binary: commands, output, config | yes |
| `ter-sdk` | TER Learn client, error envelope, version gate | yes |
| `ter-remote` | local service for the lesson page, benches | yes |
| `ter-check` | `check.yaml` evaluator | never |
| `ter-telemetry` | venues, instruments, event stream | never |
| `ter-flash` | flashing and reset | never |
| `ter-gen` | board catalog, templates, installer | |

`ter-check`, `ter-telemetry` and `ter-flash` must not depend on anything
that reaches the network; `scripts/check-deps.sh` enforces it.

```
scripts/check.sh                 fmt, clippy, tests, dependency rule
scripts/check.sh --live          plus tests against the live site (needs TER_TOKEN;
                                 TER_CURRICULUM=<checkout> adds the --dev and
                                 solution build checks; WOKWI_CLI_TOKEN with
                                 TER_CURRICULUM adds the Wokwi runs)
scripts/check.sh --install-hook  pre-commit hook running the fast part
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
