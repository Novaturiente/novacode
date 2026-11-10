# Editors (Zed and other ACP clients)

novacode can run inside an editor's agent panel instead of the terminal. It speaks the **Agent Client Protocol** (ACP) v1: the editor starts `novacode --acp` and draws the chat, the diffs and the permission prompts itself.

Zed supports ACP natively. JetBrains IDEs, Neovim (CodeCompanion, avante.nvim), Emacs (agent-shell) and VS Code (community extensions) have ACP clients too. This page is tested with Zed.

## Set up Zed

Log in once in a terminal (`novacode login`). The editor can't run the login flow. Then add novacode to Zed's `settings.json`:

```json
{
  "agent_servers": {
    "novacode": {
      "type": "custom",
      "command": "novacode",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Open the agent panel, pick **novacode** from the new-thread menu and type. Each thread is a normal novacode session in the project folder, saved like any other.

Each session needs a model. It comes from the `model` setting (see [Settings](config.md)), or add `"-m", "claude/claude-opus-5-5"` to `args` to use one model for every session.

Project settings, project MCP servers and project skills load only in folders you have trusted. Trust a folder by opening novacode in it once in a terminal and answering the trust question.

## What works in the editor

- **Chat:** replies and thinking stream in. Messages you send while the agent works are queued by Zed and sent after the turn.
- **Tools:** each call shows its title and output. `edit` and `write` show a diff you can review, and Zed's follow mode jumps to the file being read or edited.
- **Permissions:** a call that needs approval shows a card with **Allow once**, **Allow for this session**, **Always allow** (saves a rule) and **Deny**.
- **Questions:** `ask_user` opens a form with the choices and a free-text field for each question.
- **Images:** paste an image or a screenshot into the prompt. It is attached as in the terminal.
- **Files:** @-mentioned files reach the model as their paths.
- **Model, thinking and mode:** pickers in the composer change the model, the thinking level and the mode (plan mode and your mode presets).
- **Slash commands:** `/compact`, `/context`, `/model`, `/thinking`, `/mode`, skills and prompt templates all work. Command output appears as a reply.
- **Context use:** Zed shows the context in use against the model's window. Compaction shows as an entry in the thread.
- **History:** past sessions of the folder are listed in Zed's thread history and reopen with their messages and tool calls.

## What still needs the terminal

Every editor session is a live session, so `novacode attach <id>` opens it in a terminal window next to the editor. The id is the thread's session id; `novacode agents` lists the live ones. Use it for:

- steering a running turn (Zed has no steer for external agents);
- `/ps` and the side panels (processes, usage, timing, files) and extension panels;
- `/tree`, rewind, fork and `/resume`;
- login and `novacode doctor`;
- answering an `ask_user` question in an editor that has no forms.

Long `bash` output appears when the command finishes, not while it runs.

## Troubleshooting

- **The thread fails to start:** usually no model is set. Set `model` in settings or pass `-m` in `args`.
- **See the raw messages:** in Zed, run `dev: open acp logs`. novacode's own log is in `~/.novacode/logs/`.
