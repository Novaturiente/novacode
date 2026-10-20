# Install

This page shows how to install novacode, update it, remove it, and where it keeps its files.

## Platforms

| Platform | Prebuilt binary | Status |
|---|---|---|
| Linux x86_64 | yes (static, musl) | supported |
| Linux arm64 | yes (static, musl) | supported |
| macOS arm64 (Apple silicon) | yes | untested, experimental |
| Windows | no | not supported |

On Windows, use WSL. novacode runs there as a normal Linux program. Other platforms (for example macOS on Intel) have no prebuilt binary; build from source.

## Install a release

```sh
curl -fsSL https://raw.githubusercontent.com/Novaturiente/novacode/main/scripts/install.sh | sh
```

The script finds the latest release on GitHub, downloads the archive for your machine, checks its SHA-256 checksum, and puts `novacode` in `~/.local/bin`. It never touches `~/.novacode`.

| Variable | Default | Effect |
|---|---|---|
| `NOVACODE_BIN_DIR` | `~/.local/bin` | Where the binary goes |
| `NOVACODE_REPO` | `Novaturiente/novacode` | GitHub repo to download from |

If `~/.local/bin` is not on your `PATH`, the script tells you to add it. Check the result:

```sh
novacode --version
```

## Arch Linux (AUR)

Two packages exist:

| Package | What it is |
|---|---|
| `novacode` | Built from source on your machine |
| `novacode-bin` | The prebuilt static binary |

```sh
paru -S novacode        # or: paru -S novacode-bin
```

Both install to `/usr/bin/novacode`. Update them with your AUR helper, not with `novacode update`.

## With cargo

```sh
cargo install --locked --git https://github.com/Novaturiente/novacode novacode
```

This builds from source and puts the binary in `~/.cargo/bin`. You need Rust 1.97 or newer (the minimum supported version).

## Build from source

```sh
git clone https://github.com/Novaturiente/novacode
cd novacode
cargo build --release --locked -p novacode
./target/release/novacode --version
```

To install from your checkout:

```sh
cargo install --locked --path crates/novacode
```

### Static Linux binary

`scripts/build-musl.sh` builds a static x86_64 binary that runs on any Linux machine without glibc. You need the musl Rust target and a musl C compiler:

```sh
rustup target add x86_64-unknown-linux-musl
scripts/build-musl.sh            # binary: target/x86_64-unknown-linux-musl/release/novacode
scripts/build-musl.sh myserver   # also copies it to myserver:~/.local/bin/novacode over ssh
```

The script looks for `musl-gcc` on your `PATH`. Set `MUSL_CC` to use another compiler.

## Update

```sh
novacode update
```

What happens depends on how you installed:

| Installed with | `novacode update` does |
|---|---|
| Install script (release binary) | Downloads the newest release, checks the checksum, and replaces the binary in place. Running sessions keep the old version until you `/restart`. |
| `novacode-bin` or any release binary under `/usr` | Does nothing. It says the file belongs to your package manager. |
| `novacode` (AUR, source build) | Does nothing. It tells you to update with your AUR helper, for example `paru -Syu`. |
| `cargo install` or a source build | Does nothing. It tells you to run `git pull && cargo install --locked --path crates/novacode` in your checkout, or to rerun `cargo install --git` with `--force`. |

An update never touches `~/.novacode`.

## Uninstall

Remove the binary:

```sh
rm ~/.local/bin/novacode          # install script
cargo uninstall novacode          # cargo install
paru -R novacode                  # AUR (or novacode-bin)
```

Your data stays in `~/.novacode` (settings, logins, sessions). Delete that folder too if you want a clean removal. Projects may also hold a `.novacode/` folder with project settings.

## Where files live

novacode keeps everything in one folder: `~/.novacode`. Set `NOVACODE_HOME` to use another folder (for example for a second profile, or for tests).

```sh
NOVACODE_HOME=/tmp/nc-test novacode
```

| Path | Holds |
|---|---|
| `~/.novacode/settings.json` | Your settings (see [config](config.md)) |
| `~/.novacode/auth.json` | Logins and saved API keys (owner-only permissions) |
| `~/.novacode/sessions/` | One folder per project, with your saved sessions |
| `~/.novacode/trust.json` | Which project folders you trusted |
| `~/.novacode/mcp.json` | MCP servers |
| `~/.novacode/keybindings.json` | Your key bindings |
| `~/.novacode/AGENTS.md` | Instructions loaded in every project |
| `~/.novacode/extensions/` | Your extensions |
| `~/.novacode/logs/` | Daily log files (set `NOVACODE_LOG`, for example `NOVACODE_LOG=debug`, to change the level) |
| `~/.novacode/cache/` | Model catalog refresh |

In a project, `<project>/.novacode/settings.json` and friends hold project config. They only load in folders you trust. See [security](security.md).

## See also

- [Quickstart](quickstart.md)
- [Providers](providers.md)
- [Config](config.md)
- [Troubleshooting](troubleshooting.md)
