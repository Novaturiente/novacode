# Packages

A package is a folder (or git repo) that bundles extensions, skills, prompts, agents, themes and MCP servers, so you can share them and install them with one command. This page covers installing, updating and removing packages, packages a project asks for, and how to write your own.

## What a package holds

Every part is optional. Use the folder names exactly as shown.

| In the package | What it adds |
|---|---|
| `extensions/` | Script extensions (see [extensions](extensions.md)) |
| `skills/` | Agent Skills (`<name>/SKILL.md`) |
| `prompts/` | Prompt templates (`<name>.md`, used as `/name`) |
| `agents/` | Subagent definitions (see [subagents](subagents.md)) |
| `themes/` | Themes (`<name>.json`, picked with `ui.theme`) |
| `mcp.json` | MCP servers, in Claude Code's `mcpServers` format (see [mcp](mcp.md)) |
| `novacode-package.json` | Optional manifest. Only `apiVersion` is read (see [API version check](#api-version-check)) |

npm packages are not supported. A package comes from a git URL or a local folder.

A package can't carry process extensions or settings. Process extensions (and custom providers) come only from your settings; see [extensions](extensions.md).

## Install

```sh
novacode install https://github.com/you/my-pack.git   # clone it
novacode install git@github.com:you/my-pack.git       # any URL git can clone
novacode install ~/code/my-pack                       # link a local folder
novacode install ~/code/my-pack --name mine          # choose the name
```

The package name is the last part of the path without `.git` (`my-pack` above), and it is the folder name under `packages/`. A name can't contain `/` or start with a dot. Use `--name` when you want another name. If a package of that name is already installed, the command stops and tells you to uninstall it first.

Installs go to `~/.novacode/packages/<name>`. New sessions load the package; run `/reload` in a running session to pick it up.

| Source | What happens |
|---|---|
| Git URL | novacode runs `git clone` and records the exact commit in `~/.novacode/packages.lock.json`. |
| Local folder | novacode makes a symlink to it. Your edits apply at once. Nothing is recorded in the lock file. |

If a path exists as a folder on disk, novacode treats it as a local folder. Otherwise it treats the text as a git URL. You need `git` on your `PATH` for git installs. Git never asks for a password in the terminal (`GIT_TERMINAL_PROMPT=0`), so private repos need an SSH key or a credential helper.

## List, update, remove

```sh
novacode packages                 # list what's installed
novacode packages update          # move every cloned package to its newest commit
novacode packages update my-pack  # only this one
novacode uninstall my-pack        # remove it
```

`novacode packages` shows each package with the parts it contains and where it comes from (`git`, or `→ /path` for a linked folder). A package that can't load shows `(disabled: …)` with the reason.

`packages update` pulls the default branch (fast-forward only) and writes the new commit into the lock file. Linked folders are always current, so update skips them. `uninstall` deletes a clone, or removes just the symlink for a linked folder. It never touches your source folder.

`install`, `uninstall` and `packages update` work on your own packages in `~/.novacode/`. They don't change a project's packages.

## Pinned commits

The lock file `packages.lock.json` maps each cloned package to its source and commit:

```json
{
  "my-pack": {
    "source": "https://github.com/you/my-pack.git",
    "rev": "3f2a9c1d0b5e7a8c4d6e1f2a3b4c5d6e7f8a9b0c"
  }
}
```

Nothing changes under you. A package stays at its recorded commit until you run `novacode packages update`.

## Packages for a project

A repo can list the packages its team uses. Create `.novacode/packages.json`:

```json
{
  "packages": {
    "team-tools": "https://github.com/acme/team-tools.git"
  }
}
```

The key is the package name and the value is the source (a git URL or a local path). Commit this file and `.novacode/packages.lock.json`. The installed copies live in `.novacode/packages/`, so add that folder to `.gitignore`.

Project packages only load in a [trusted folder](security.md). When you open a trusted project that lists packages you don't have:

- In the TUI, novacode asks once: install them, or "Not now (don't ask again here)". The message warns that packages can add tools, hooks and MCP servers that run code. Your answer is kept per project in `~/.novacode/project-packages.json`.
- In print mode, RPC and other non-interactive runs, it prints a note and installs nothing.
- Nothing is installed without your yes.

To install them yourself at any time:

```sh
novacode trust                # once per project, if you haven't yet
novacode packages install
```

`packages install` installs each missing package at the commit in `.novacode/packages.lock.json`, so everyone on the team gets the same code. If the lock file has no entry for a package (or its source changed), it takes the newest commit and records it. Run `novacode packages` inside the project to see project packages marked with a `project` prefix.

## Which one wins

A package never overrides your own files. For skills, prompts and agents, the first folder with a name wins: your project's `.novacode/`, the project's `.claude/`, `~/.novacode/`, `~/.claude/`, `~/.agents/skills` (skills only), and last the packages. Project packages come before your global packages. [Skills](skills.md#where-skills-are-found) has the full order.

When two packages use the same skill, prompt or agent name, the first one wins and the other is skipped without a message. `novacode doctor` lists what was skipped. Extension files that clash are settled the same way: your project, then `~/.novacode`, then packages.

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

2. If you ship MCP servers, `mcp.json` uses the same format as `~/.novacode/mcp.json`:

   ```json
   {
     "mcpServers": {
       "docs": { "command": "npx", "args": ["-y", "some-docs-server"] }
     }
   }
   ```

   The server starts in the user's session folder, not in your package folder, and there is no variable for the package path. Use commands that are on the `PATH`.

3. Try it from the folder while you work. The link means every edit applies at once:

   ```sh
   novacode install ./my-pack
   novacode packages
   ```

4. Publish it as a git repo:

   ```sh
   cd my-pack
   git init && git add . && git commit -m "first version"
   git remote add origin git@github.com:you/my-pack.git
   git push -u origin main
   ```

5. Others install it with `novacode install https://github.com/you/my-pack.git`.

Remember that a package can run code on the machine of whoever installs it (extensions, MCP servers). Say what it does in the README, and read other people's packages before you install them.

## See also

- [extensions](extensions.md): writing the scripts that go in `extensions/`
- [skills](skills.md): skills, prompts and how they load
- [subagents](subagents.md): agent definitions for `agents/`
- [mcp](mcp.md): MCP servers and `mcp.json`
- [security](security.md): trust, which decides whether project packages load
- [config](config.md): settings such as `ui.theme`
