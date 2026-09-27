# ter

`ter` is the command line tool for [TER Learn](https://learn.theembeddedrustacean.com),
The Embedded Rustacean's course site. It pairs your machine with your TER
Learn account, fetches exercises as small Cargo projects, builds and runs
them on your board or in a simulator, checks what the program did, and
records the attempt on the site.

This is an early version: pairing (`ter login`, `ter logout`,
`ter whoami`), exercises (`ter ex`), build-only runs (`ter run --no-check`),
`ter status`, `ter hint` and `ter self-update` work today. Running on a
board or in a simulator comes next.

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
ter run --no-check    build the exercise and record the attempt on the site
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

`ter run --no-check` in an exercise folder builds it (into the course's
shared cache) and posts the attempt to TER Learn: a failed build with the
end of the compiler output, a good build as built but not checked. `--sim`
or `--hw` picks the mode for one run instead of `ter.toml`'s. Each run's
build log is kept in `.runs/<n>/build.log` in the exercise.

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
                                 solution build checks)
scripts/check.sh --install-hook  pre-commit hook running the fast part
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
