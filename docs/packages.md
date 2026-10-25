# Packages

A package is a folder or git repo that bundles skills, prompt templates, subagents, MCP servers, hooks, extensions and themes, so you can share them and install them with one command. novacode reads two kinds:

- **novacode packages**, laid out as described in [What a package holds](#what-a-package-holds).
- **Claude Code plugins**, such as [ponytail](https://github.com/DietrichGebert/ponytail), [caveman](https://github.com/juliusbrussee/caveman) or [mattpocock/skills](https://github.com/mattpocock/skills). They install as they are; see [Claude Code plugins](#claude-code-plugins).

This page covers installing, versions and updates, removing packages, packages a project asks for, and how to write your own.

## Install

```sh
novacode install https://github.com/you/my-pack.git         # its latest release
novacode install git@github.com:you/my-pack.git             # any URL git can clone
novacode install https://github.com/you/my-pack --ref v1.2.0  # a tag, branch or commit
novacode install ~/code/my-pack                             # link a local folder
novacode install ~/code/my-pack --name mine                 # choose the name
```

The package name is the last part of the path without `.git` (`my-pack` above), and it is the folder name under `packages/`. A name can't contain `/` or start with a dot. Use `--name` when you want another name. If a package of that name is already installed, the command stops and tells you to uninstall it first.

Installs go to `~/.novacode/packages/<name>`. New sessions load the package; run `/reload` in a running session to pick it up. After the install, novacode prints what the package adds (for example `Claude Code plugin 5.3.2: 6 skills` and the hook events it runs) and asks for its [options](#plugin-options) if it has any.

| Source | What happens |
|---|---|
| Git URL | novacode clones it and checks out its **latest release** (see [Versions](#versions-and-updates)). The commit and tag go into `~/.novacode/packages.lock.json`. |
| Local folder | novacode makes a symlink to it. Your edits apply at once. Nothing is recorded in the lock file, and `--ref` is ignored. |

If a path exists as a folder on disk, novacode treats it as a local folder. Otherwise it treats the text as a git URL. You need `git` on your `PATH` for git installs. Git never asks for a password in the terminal (`GIT_TERMINAL_PROMPT=0`), so private repos need an SSH key or a credential helper.

## Versions and updates

Packages follow **releases**, not every commit.

The latest release is:

1. For a GitHub repo, the release GitHub marks as "Latest". novacode asks GitHub's public API, which needs no token or setup.
2. Otherwise (another host, no network, or the API limit was hit), the highest version tag: `v1.2.3` or `1.2.3`, also with a name prefix such as `bin-v1.2.3`. Pre-releases like `v2.0.0-rc1` are skipped.
3. A repo with no release tags at all is installed at its newest commit and then **stays pinned** there.

```sh
novacode packages update                 # every package to its latest release
novacode packages update my-pack         # only this one
novacode packages update --commits       # also move packages without releases to their newest commit
```

What `packages update` does, by how the package was installed:

| Installed as | `packages update` |
|---|---|
| Latest release (the default) | Moves to the latest release as defined above, but never to an older release. If GitHub's "Latest" is older than the installed tag, the package stays where it is. |
| `--ref <tag>` | The same: a pinned tag moves to a newer latest release, never to an older one. Reinstall with `--ref` to hold a tag. |
| `--ref <branch>` | Follows the branch: moves to its newest commit. |
| `--ref <commit>` | Stays only while the repo has no releases. Once it has any, a plain update moves it to the latest release. |

To tell older from newer, novacode compares tag versions (`v1.10.0` is newer than `v1.9.0`). If the installed tag isn't a version (for example `nightly` or `v2.0.0-rc1`), it can't be compared, and the package moves to the latest release. If the installed tag is a version but the latest release isn't, the package stays.
| A repo without releases | Stays where it is. `--commits` moves it to the newest commit of the default branch. If the repo later starts publishing releases, a plain update moves it to the latest one. |

The command prints one line per package, for example `ponytail: v5.3.1 → v5.3.2`, `caveman: up to date (v4.0.1)` or ``my-pack: no releases; stays at 3f2a9c1d0b5e (`--commits` moves it to the newest commit)``. Linked folders are always current, so update skips them. `packages update` exits `1` if any package failed. A name that isn't installed prints nothing. `packages install` needs a [trusted](security.md) folder and stops with ``this folder isn't trusted; run `novacode trust` first`` otherwise.

`install`, `uninstall`, `packages update` and `packages config` work on your own packages in `~/.novacode/`. They don't change a project's packages.

### The lock file

`packages.lock.json` maps each cloned package to its source, commit, and the tag or branch it follows:

```json
{
  "ponytail": {
    "source": "https://github.com/DietrichGebert/ponytail",
    "rev": "16352ac229eb08a9598dcae48fe9f2b342567d86",
    "tag": "v5.3.2"
  }
}
```

Nothing changes under you. A package stays at its recorded commit until you run `novacode packages update`. If the file isn't valid JSON, `install`, `uninstall`, `packages update` and `packages install` stop with an error that names the file. novacode leaves the damaged file as it is; fix or delete it, then run the command again. `novacode packages` still lists the packages, without their versions. Lock files from older novacode versions (without `tag`) keep working: a plain update moves such a package to its latest release if it has one.

## List and remove

```sh
novacode packages                 # list what's installed
novacode uninstall my-pack        # remove it
novacode uninstall https://github.com/you/my-pack.git   # or by the URL or path you installed from
```

`novacode packages` shows each package with its version (`git v5.3.2`, or `→ /path` for a linked folder), what kind it is, what it adds, and which hook events it runs. A package that can't load shows `(disabled: …)` with the reason. Hooks or hook events novacode doesn't support are listed as `not supported`.

`uninstall` deletes a clone, or removes just the symlink for a linked folder. It never touches your source folder. It keeps `plugin-data/<name>/` and the package's saved options in `package-options.json`; delete those by hand to remove every trace.

## What a package holds

A novacode package uses these folders. Every part is optional. Use the names exactly as shown.

| In the package | What it adds |
|---|---|
| `extensions/` | Script extensions (see [extensions](extensions.md)) |
| `skills/` | Agent Skills (`<name>/SKILL.md`, at any depth) |
| `prompts/` | Prompt templates (`<name>.md`, used as `/name`) |
| `agents/` | Subagent definitions (see [subagents](subagents.md)) |
| `themes/` | Themes (`<name>.json`, picked with `ui.theme`) |
| `mcp.json` | MCP servers, in Claude Code's `mcpServers` format (see [mcp](mcp.md)) |
| `novacode-package.json` | Optional manifest. Only `apiVersion` is read (see [API version check](#api-version-check)) |

npm packages are not supported. A package comes from a git URL or a local folder.

A package can't carry process extensions or settings. Process extensions (and custom providers) come only from your settings; see [extensions](extensions.md).

## Claude Code plugins

A repo is read as a Claude Code plugin when it has a valid `.claude-plugin/plugin.json` (one that isn't valid JSON counts as missing), or, without a `novacode-package.json`, any of `commands/`, `hooks/hooks.json` or `.mcp.json`. Its parts then follow Claude Code's rules:

| Plugin part | In novacode |
|---|---|
| `skills/<name>/SKILL.md` (one level deep), plus the folders the manifest's `skills` lists, or a single `SKILL.md` at the root | Skills |
| `commands/*.md`, or the manifest's `commands` (paths or a name → `source` map) | Prompt templates (`/name`); `$ARGUMENTS` works |
| `agents/*.md`, or the manifest's `agents` | Subagent types |
| `.mcp.json`, plus the manifest's `mcpServers` (a `.json` path or inline servers) | MCP servers |
| `hooks/hooks.json`, plus the manifest's `hooks` (a path, inline, or a list of both) | Hooks, see below |
| `userConfig` in the manifest | [Options](#plugin-options) |

Other files (`.codex-plugin/`, `.cursor-plugin/`, `.toml` commands, installers, source code) are ignored.

These variables are filled in, as Claude Code does:

| Variable | Means |
|---|---|
| `${CLAUDE_PLUGIN_ROOT}` | The package folder (`~/.novacode/packages/<name>`). |
| `${CLAUDE_PLUGIN_DATA}` | `~/.novacode/plugin-data/<name>/`, kept across updates and created when a session loads the plugin. |
| `${CLAUDE_PROJECT_DIR}` | The session's folder (hooks only). |
| `${user_config.KEY}` | An option's value. Secret (`sensitive`) options are filled in only for hooks and MCP servers, never in text the model reads. |

They are filled in for MCP server configs, hook commands, `/skill:` and command text, and agent prompts. When the model reads a `SKILL.md` on its own, it sees `${CLAUDE_PLUGIN_ROOT}` as written; the skills list tells it that the variable means the package's folder.

### Hooks

A plugin's command hooks run with Claude Code's input on stdin (`session_id`, `cwd`, `hook_event_name`, plus the event's fields) and Claude Code's output rules: `hookSpecificOutput.additionalContext` adds context (for `SessionStart`, `SubagentStart` and `UserPromptSubmit`, plain stdout does too), exit code `2` or `"decision": "block"` blocks. Tool names are Claude Code's (`Bash`, `Edit`, `Read`, `mcp__<server>__<tool>`), and a hook's `matcher` applies to them. For `PostToolUse` and `PreToolUse`, plain stdout is ignored; only the JSON fields below count. Hooks get `CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_DATA`, `CLAUDE_PROJECT_DIR` and `CLAUDE_PLUGIN_OPTION_<KEY>` (the key in upper case) in their environment. Both forms work: shell (`command`, run with `sh`, or `bash` with `"shell": "bash"`) and exec (`command` plus `args`). `timeout` (seconds) and `async: true` are honoured.

| Event | When it runs in novacode | What its output does |
|---|---|---|
| `SessionStart` | Before the first model call of a session (`startup`, or `resume` for a resumed one), and after each compaction (`compact`) | Saved as context for the model |
| `SubagentStart` | Instead of `SessionStart` in a subagent; the matcher is the agent type | Saved as context for the subagent |
| `UserPromptSubmit` | Before each prompt you send (not in subagents) | Context is saved after the prompt; a block refuses the prompt and shows the reason |
| `PreToolUse` | Before each tool call | Block or `permissionDecision: "deny"` (the reason goes to the model); `"ask"` needs your approval even when the policy allows the call (refused when nobody can approve); `"allow"` skips only `ask` rules and ask mode, never deny rules, protected commands, the risk gate or read-only mode. Deny beats ask, ask beats allow. `updatedInput` changes the arguments |
| `PostToolUse` | After each tool call | `additionalContext`, a block reason or exit-2 stderr is added to the result; `updatedToolOutput` replaces it |
| `Stop` | When the agent finishes a turn normally (not in subagents) | A block keeps the agent going, with the reason as its next instruction; `stop_hook_active` is `true` on the next stop |
| `SessionEnd` | When the session closes | Nothing (observe only) |

Plugin hooks run every time their event fires, with your permissions, from the moment the package is installed. novacode does not ask before running them. Read a plugin's hooks before you install it, and check what changed before you update. `novacode packages` lists which events each package hooks.

Every hook fails open: if it crashes, times out (default 30 s for `SessionStart`, `SubagentStart` and `UserPromptSubmit`, 60 s for the rest, 5 s for `SessionEnd`) or prints something novacode can't read, nothing changes.

Not supported: hook types other than `command` (`prompt`, `agent`, `http`, `mcp_tool`), other events (`PreCompact`, `UserPromptExpansion`, `Notification`, …), `transcript_path` (novacode's session file isn't Claude Code's format), and `bin/`, `.lsp.json`, output styles, monitors, workflows, channels, `.mcpb` bundles and `dependencies`. `novacode packages` and `novacode doctor` list skipped hooks per package. A marketplace repo that holds many plugins (such as `wshobson/agents`) is not split up; install a plugin's own repo instead.

### Plugin options

A plugin can declare options (`userConfig` in its manifest), such as a webhook URL or an API token. `novacode install` asks for each one in the terminal; press Enter to keep the default. Without a terminal, the defaults are used. Change them later with:

```sh
novacode packages config ponytail
```

Values are kept in `~/.novacode/package-options.json`, readable only by you (like `auth.json`). Secret options are masked while you type.

## Packages for a project

A repo can list the packages its team uses. Create `.novacode/packages.json`:

```json
{
  "packages": {
    "team-tools": "https://github.com/acme/team-tools.git"
  }
}
```

The key is the package name and the value is the source (a git URL or a local path). Commit this file and `.novacode/packages.lock.json`. The installed copies live in `.novacode/packages/` (plugin data in `.novacode/plugin-data/`, options in `.novacode/package-options.json`), so add those to `.gitignore`.

Project packages only load in a [trusted folder](security.md). When you open a trusted project that lists packages you don't have:

- In the TUI, novacode asks once: install them, or "Not now (don't ask again here)". The message warns that packages can add tools, hooks and MCP servers that run code. Your answer is kept per project in `~/.novacode/project-packages.json`.
- In print mode, RPC and other non-interactive runs, it prints a note and installs nothing. The note also appears in the TUI on later runs once you have answered.
- Nothing is installed without your yes.

To install them yourself at any time:

```sh
novacode trust                # once per project, if you haven't yet
novacode packages install
```

`packages install` installs each missing package at the commit in `.novacode/packages.lock.json`, so everyone on the team gets the same code. If the lock file has no entry for a package (or its source changed), it takes the latest release and records it. Run `novacode packages` inside the project to see project packages marked with a `project` prefix.

## Which one wins

A package never overrides your own files. For skills, prompts and agents, the first folder with a name wins: your project's `.novacode/`, the project's `.claude/`, `~/.novacode/`, `~/.claude/`, `~/.agents/skills` (skills only), and last the packages. Project packages come before your global packages. [Skills](skills.md#where-skills-are-found) has the full order.

A package's skill, prompt or agent whose name is already taken is not lost: it loads as `<package>:<name>`. For example, if you have your own `retro` skill and install `mattpocock/skills`, its skill is `/skill:skills:retro`. `novacode doctor` lists every clash and the name it loads under. Extension files that clash are settled by order alone: your project, then `~/.novacode`, then packages.

For MCP servers, packages are loaded first, then `~/.novacode/mcp.json`, then the project's `.mcp.json`. Later files win a server name. Between packages, a project package beats a global one, and within the same scope the name that comes first alphabetically wins. See [mcp](mcp.md).

Themes are found in `~/.novacode/themes` first. If your `ui.theme` name isn't there, novacode looks in your installed (global) packages. Themes in project packages are not searched.

## API version check

An extension talks to novacode through a versioned API (currently `1`). A package can say which version it was written for in `novacode-package.json`:

```json
{ "apiVersion": [1, 2] }
```

Other keys, such as `name` and `version`, are allowed but ignored. The package's name is always its folder name.

`apiVersion` is a number (`1`) or a `[min, max]` pair. If this novacode's API is outside the range, the package is not loaded. `novacode packages` and `novacode doctor` show why, for example `it needs extension API 2, this novacode has 1`. Without a manifest, or without `apiVersion`, the package always loads. A manifest that isn't valid JSON, or an `apiVersion` that isn't whole numbers, also disables the package with a reason.

## Make your own package

1. Make a folder and add only the parts you need:

   ```text
   my-pack/
   ├── novacode-package.json      # optional
   ├── skills/
   │   └── release-notes/
   │       └── SKILL.md
   ├── prompts/
   │   └── review.md
   ├── agents/
   ├── extensions/
   │   └── hello.ts
   ├── themes/
   └── mcp.json
   ```

   If you want the same repo to work in Claude Code too, use the Claude Code plugin layout instead (`.claude-plugin/plugin.json`, `skills/`, `commands/`, `agents/`, `hooks/hooks.json`, `.mcp.json`). novacode reads it as described in [Claude Code plugins](#claude-code-plugins), but script extensions are then not loaded. A `themes/<name>.json` still works.

2. If you ship MCP servers, `mcp.json` uses the same format as `~/.novacode/mcp.json`:

   ```json
   {
     "mcpServers": {
       "docs": { "command": "npx", "args": ["-y", "some-docs-server"] }
     }
   }
   ```

   The server starts in the user's session folder, not in your package folder. In a novacode package, use commands that are on the `PATH`. In a Claude Code plugin, `${CLAUDE_PLUGIN_ROOT}` points at the package folder.

3. Try it from the folder while you work. The link means every edit applies at once:

   ```sh
   novacode install ./my-pack
   novacode packages
   ```

4. Publish it as a git repo, and tag releases so users get stable versions:

   ```sh
   cd my-pack
   git init && git add . && git commit -m "first version"
   git remote add origin git@github.com:you/my-pack.git
   git push -u origin main
   git tag v0.1.0 && git push origin v0.1.0
   ```

5. Others install it with `novacode install https://github.com/you/my-pack.git`, and `novacode packages update` moves them to GitHub's "Latest" release, or else to your highest version tag.

Remember that a package can run code on the machine of whoever installs it (extensions, hooks, MCP servers). Say what it does in the README, and read other people's packages before you install them.

## See also

- [extensions](extensions.md): writing the scripts that go in `extensions/`
- [skills](skills.md): skills, prompts and how they load
- [subagents](subagents.md): agent definitions for `agents/`
- [mcp](mcp.md): MCP servers and `mcp.json`
- [automation](automation.md): command hooks in your own settings
- [security](security.md): trust, which decides whether project packages load
- [config](config.md): settings such as `ui.theme`
