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

The script finds the latest release on GitHub, downloads the archive for your machine, checks its SHA-256 checksum, and puts `novacode` in `~/.local/bin`. It never touches `~/.novacode`. It needs `curl`, `tar` and `sha256sum` (or `shasum`). It asks GitHub's API without a token, so heavy use from one address can hit a rate limit. Prereleases are ignored.

| Variable | Default | Effect |
|---|---|---|
| `NOVACODE_BIN_DIR` | `~/.local/bin` | Where the binary goes |
| `NOVACODE_REPO` | `Novaturiente/novacode` | GitHub repo to download from |

If `~/.local/bin` is not on your `PATH`, the script tells you to add it. Add this line to `~/.bashrc` or `~/.zshrc` and open a new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Check the result:

```sh
novacode --version
```

## Arch Linux (AUR)

The repo has build files for two packages in `packaging/aur/`. They are not published on the AUR yet, so build them with `makepkg`:

| Package | What it is |
|---|---|
| `novacode` | Built from source on your machine |
| `novacode-bin` | The prebuilt static binary |

```sh
git clone https://github.com/Novaturiente/novacode
cd novacode/packaging/aur/novacode-bin   # or: novacode
makepkg -si
```

Both install to `/usr/bin/novacode`. To update, pull the repo and run `makepkg -si` again; `novacode update` leaves them alone.

## With cargo

```sh
cargo install --locked --git https://github.com/Novaturiente/novacode novacode
```

This builds from source and puts the binary in `~/.cargo/bin`. You need Rust 1.97 or newer (the minimum supported version), `git` and a C compiler (novacode builds SQLite and QuickJS from C).

## Build from source

You need the same as for cargo: Rust 1.97+, `git` and a C compiler.

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

The script looks for `musl-gcc` on your `PATH`, then for `~/.local/musl/bin/musl-gcc`. Set `MUSL_CC` to use another compiler.

## Update

```sh
novacode update
```

What happens depends on how you installed:

| Installed with | `novacode update` does |
|---|---|
| Install script (release binary) | Downloads the newest release, checks the checksum, and replaces the binary in place. Running sessions keep the old version until you `/restart`. |
| `novacode-bin` or any release binary in a folder under `/usr` (even `/usr/local/bin`) | Does nothing. It says the file belongs to your package manager. |
| `novacode` (AUR, source build) | Does nothing. It tells you to update with your AUR helper, for example `paru -Syu`. |
| `cargo install` or a source build | Does nothing. It tells you to run `git pull && cargo install --locked --path crates/novacode` in your checkout, or to rerun `cargo install --git` with `--force`. |

If you are already current it says `novacode X is the newest release`. The binary's folder must be writable by you, because the update replaces the file. Updates use GitHub's latest (non-prerelease) release. An update never touches your files in `~/.novacode`. The docs ship inside the binary: the first run of a new version rewrites `~/.novacode/docs/` to match it, whichever way you installed.

### Update check

An interactive session tells you when a newer release is out, however you installed:

```
novacode v0.2.6 is available (you have 0.2.3): run `novacode update`
```

- novacode asks GitHub for its latest release **at most once a day**, in the background. It caches the answer in `~/.novacode/cache/updates.json`.
- Every interactive start shows the note from the cache, until you have updated. `/reload`, `/new` and `/resume` don't show it again.
- The same check covers your packages (see [Packages](packages.md#update-check)).
- It is one GitHub API call for novacode plus one per package. It sends only the request itself (user agent `novacode/<version>`), no identifiers.
- A check where every request fails stays silent and tries again at the next start. If only some requests fail, the rest is cached and the next check is a day later.
- Nothing is installed: you run `novacode update` yourself.
- `-p`, `serve` and subagents never check.

To turn it off, set `"updates": {"check": false}` in `~/.novacode/settings.json`.

## Uninstall

Remove the binary:

```sh
rm ~/.local/bin/novacode          # install script
cargo uninstall novacode          # cargo install
sudo pacman -R novacode           # Arch package (or novacode-bin)
```

Your data stays in `~/.novacode` (settings, logins, sessions). Delete that folder too if you want a clean removal. Projects may also hold a `.novacode/` folder with project settings.

## Where files live

novacode keeps everything in one folder: `~/.novacode`. Set `NOVACODE_HOME` to use another folder (for example for a second profile, or for tests). If neither `NOVACODE_HOME` nor `HOME` is set, novacode stops with "cannot find home dir".

```sh
NOVACODE_HOME=/tmp/nc-test novacode
```

| Path | Holds |
|---|---|
| `~/.novacode/settings.json` | Your settings (see [config](config.md)) |
| `~/.novacode/auth.json` | Logins and saved API keys (owner-only permissions) |
| `~/.novacode/sessions/` | One folder per project, with your saved sessions |
| `~/.novacode/trust.json` | Which project folders you trusted |
| `~/.novacode/project-packages.json` | Your yes or no, per project folder, to installing the packages it declares |
| `~/.novacode/mcp.json` | MCP servers |
| `~/.novacode/keybindings.json` | Your key bindings |
| `~/.novacode/mcp-auth.json` | OAuth logins for MCP servers |
| `~/.novacode/AGENTS.md` | Instructions loaded in every project |
| `~/.novacode/SYSTEM.md`, `APPEND_SYSTEM.md` | Replace or extend the system prompt (see [context](context.md)) |
| `~/.novacode/extensions/` | Your extensions |
| `~/.novacode/skills/`, `prompts/`, `agents/`, `themes/` | Your skills, prompt templates, subagent definitions and themes |
| `~/.novacode/packages/`, `packages.lock.json` | Installed packages and their pinned commits |
| `~/.novacode/memory/` | Memory that carries across sessions |
| `~/.novacode/checkpoints/` | File checkpoints for `/rewind` |
| `~/.novacode/spill/`, `processes/`, `goals/` | Long tool output, background process records, `/goal` state |
| `~/.novacode/plugin-data/`, `package-options.json` | Data and options of installed packages |
| `~/.novacode/exhausted.json` | Which subscription accounts are used up, and until when |
| `~/.novacode/metrics/` | Timing files, when `debug.metrics` is on |
| `~/.novacode/logs/` | Daily log files (set `NOVACODE_LOG`, for example `NOVACODE_LOG=debug`, to change the level), plus crash, MCP server, extension and trace logs (see [troubleshooting](troubleshooting.md)) |
| `~/.novacode/cache/` | Caches: model catalog, subscription quota, MCP tool lists and signatures, the update check (`updates.json`), built extensions and learned tool-schema workarounds |
| `*.bak`, `*.lock` | Next to files novacode writes, such as `auth.json.bak` and `settings.json.lock`: the previous version of the file, and the lock that stops two novacode processes writing it at once |
| `~/.novacode/run/` | Records of live sessions (used by `agents`, `attach`, `status`) |
| `~/.novacode/docs/` | This documentation for the installed version (rewritten on every update; don't edit it). `/docs` points a session at it |

In a project, `<project>/.novacode/settings.json` and friends hold project config. They only load in folders you trust. See [security](security.md).

## See also

- [Quickstart](quickstart.md)
- [Providers](providers.md)
- [Config](config.md)
- [Troubleshooting](troubleshooting.md)
