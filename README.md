# ter

`ter` is the command line tool for [TER Learn](https://learn.theembeddedrustacean.com),
The Embedded Rustacean's course site. It pairs your machine with your TER
Learn account, fetches exercises as small Cargo projects, builds and runs
them on your board or in a simulator, checks what the program did, and
records the attempt on the site.

This is an early version: `ter login`, `ter logout`, `ter whoami` and
`ter self-update` work today.

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
scripts/check.sh --live          plus tests against the live site (needs TER_TOKEN)
scripts/check.sh --install-hook  pre-commit hook running the fast part
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
