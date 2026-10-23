# Skills and prompt templates

This page covers two ways to teach novacode your own routines: skills (instructions the model loads when a task needs them) and prompt templates (shortcuts you type as `/name`).

## Skills

A skill is a folder with a `SKILL.md` file. It follows the Agent Skills layout, so skills written for Claude Code or pi work as they are.

```
~/.novacode/skills/
  release-notes/
    SKILL.md
    template.md        # extra files the skill can refer to
```

```markdown
---
name: release-notes
description: Write release notes from the git log. Use when the user asks for a changelog or release notes.
---

1. Run `git log --oneline` since the last tag.
2. Group the commits by type.
3. Write the notes using `template.md` in this folder.
```

### Frontmatter fields

novacode reads these fields, with the meaning Claude Code gives them. `license`, `compatibility`, `metadata` and `argument-hint` are accepted and have no effect, as in Claude Code.

| Field | Required | Meaning |
|---|---|---|
| `description` | yes | What the skill does and when to use it. A `SKILL.md` without one is skipped, and novacode warns about it. A loose `.md` file without one is skipped silently. Long descriptions are cut to 1024 characters. |
| `when_to_use` | no | Added after the description in the skill list. |
| `name` | no | The skill's name. Defaults to the folder name (or the file name for a loose `.md`). If it differs from the folder name, the system prompt shows the file's path next to it. |
| `disable-model-invocation` | no | `true` hides the skill from the model. Only you can use it, with `/skill:name`. |
| `user-invocable` | no | `false` makes it the model's only: no `/skill:` completion, and typing it does nothing. |
| `allowed-tools` | no | Tools that run without asking for the rest of the turn the skill is activated in (until your next message), e.g. `Bash(git commit *) Read`. They never get past `deny` rules, the risk gate or read-only mode. |
| `disallowed-tools` | no | Tools refused for the rest of that turn. |
| `context`, `agent` | no | `context: fork` runs the skill as a subagent of type `agent` (default `worker`) with the skill's text as its task. |

`allowed-tools`, `disallowed-tools` and `context: fork` apply when the model activates the skill with the `skill` tool. A skill whose file the model just reads gets its text only.

Not supported (listed by `novacode doctor`): `model`, `effort`, `hooks`, `paths`, `shell`, `background`, and `!` command lines in the text (they would run shell commands from the skill before the model sees it).

The frontmatter is YAML: lists, nested maps and `yes`/`no` booleans work. It must start on the very first line of the file, between two `---` lines. Frontmatter that isn't valid YAML (often an unquoted `: ` inside a description) is read as plain `key: value` lines instead, and `novacode doctor` says so. In the text, `$ARGUMENTS` (and `$1`, `$2`…) take what you type after `/skill:name`.

### Where skills are found

novacode looks in these folders. When two skills have the same name, the earlier folder wins.

| Order | Folder | Condition |
|---|---|---|
| 1 | `<project>/.novacode/skills/` | The project is trusted. |
| 2 | `<project>/.claude/skills/` | The project is trusted and `compat.claude` is on. |
| 3 | `~/.novacode/skills/` | Always. |
| 4 | `~/.claude/skills/` | `compat.claude` is on. |
| 5 | `~/.agents/skills/` | Always. |
| 6 | `skills/` inside each enabled [package](packages.md) (for a Claude Code plugin: `skills/<name>/` one level deep plus the folders its manifest lists) | Project packages only when trusted. A package skill whose name is taken loads as `<package>:<name>`. |

`compat.claude` is `true` by default. Set it to `false` to ignore every `.claude` folder. See [config](config.md).

Project folders are skipped until you trust the project, because a skill is instructions the model will follow. See [security](security.md) for how to trust a folder (`novacode trust`, or `--trust` for one run).

Inside a skills folder:

- A folder that contains `SKILL.md` is one skill. novacode does not look deeper inside it.
- Other folders are searched for `SKILL.md` files.
- A loose `.md` file directly in the skills folder (with a `description`) is also a skill. Its name is the file name without `.md`.
- Names that start with `.` and `node_modules` are skipped.

If the same real file is reachable twice (a symlink, two folders pointing to one place), it loads once, silently.

### How the model uses a skill

The system prompt gets a compact list: one line per skill with its name and its description, cut to 200 characters. The list is grouped by folder so paths are not repeated:

```
Skills: specialized instructions for specific tasks. ...
~/.novacode/skills/
- release-notes: Write release notes from the git log. Use when the user asks for a changelog or release notes.
```

The model activates a skill with the `skill` tool when the task matches: the tool returns the full text and applies the skill's tool rules. It may also just read the `SKILL.md` file. Only the short list costs tokens on every request. Relative paths inside a skill are resolved against the skill's own folder.

Add or edit a skill and the change reaches the model on its next turn, as a small prompt update.

### Using a skill yourself

Type `/skill:` and the name, then anything you want to add:

```
/skill:release-notes for v2.3
```

novacode inlines the skill's text (without the frontmatter) into your message, in a `<skill name=… location=…>` block that says references are relative to the skill's folder, then your extra text after it. This also works for skills with `disable-model-invocation: true`. The editor completes `/skill:` names for you.

### Shadowed skills

If two different files have the same name, the later one is ignored. To see what was ignored, run:

```
novacode doctor
```

The skills, prompts, agents section lists lines like `skill release-notes: /path/b/SKILL.md is shadowed by /path/a/SKILL.md`, and any package that did not load.

## Prompt templates

A prompt template is a markdown file you call as `/name`. Its text is sent as your message.

```markdown
---
description: Review the staged changes
---
Review the staged changes. Focus on $1. Extra notes: ${@:2}
```

Save it as `~/.novacode/prompts/review.md`, then type:

```
/review security "the auth module"
```

### Where templates are found

The first folder with a given name wins.

| Order | Folder | Condition |
|---|---|---|
| 1 | `<project>/.novacode/prompts/` | Always (templates are plain text). |
| 2 | `<project>/.claude/commands/` | The project is trusted and `compat.claude` is on. |
| 3 | `~/.novacode/prompts/` | Always. |
| 4 | `~/.claude/commands/` | `compat.claude` is on. |
| 5 | `prompts/` inside each enabled [package](packages.md) (`commands/` for a Claude Code plugin) | Project packages only when trusted. |

Only `.md` files directly in these folders count. Subfolders are not searched, so there are no namespaced commands like `/team:review`. The command name is the file name without `.md`.

The description shown in the editor's completion list is the `description` frontmatter field. Without one, it is the first line of the file, cut to 60 characters.

### Arguments

Everything after `/name` is split into words. Quotes group words (`"the auth module"` is one argument; `""` is an empty one).

| Placeholder | Replaced with |
|---|---|
| `$1`, `$2`, … | The first, second, … argument. Empty if missing. |
| `$@` or `$ARGUMENTS` | All arguments, joined with spaces. |
| `${1:-default}` | Argument 1, or `default` if it is missing or empty. |
| `${@:-default}` | All arguments, or `default` if there are none. |
| `${@:N}` | Arguments from the Nth on. |
| `${@:N:L}` | `L` arguments starting at the Nth. |

Substituted values are not expanded again. A `$` that matches none of these is left as it is, except that a number after it (like the `$5` in `cost $5`) is read as an argument.

A name that is not a known template is sent to the model as typed. The same goes for an unknown `/skill:` name.

## See also

- [Context and memory](context.md)
- [Packages](packages.md)
- [Subagents](subagents.md)
- [Security](security.md)
- [Migrating from other tools](migrating.md)
- [Config](config.md)
