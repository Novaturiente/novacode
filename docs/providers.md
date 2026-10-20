# Providers and models

This page shows how to connect novacode to a model: with a subscription login, with an API key, with a local server, or with your own program. It also covers choosing models and thinking levels.

A model is always written `provider/model`, for example `claude/claude-opus-5-5`. The provider is the part before the first `/`.

## Subscription logins

Four providers use your existing subscription instead of an API key:

| Provider | Subscription | How you sign in |
|---|---|---|
| `claude` | Claude Pro / Max | Open a URL, copy the code shown, paste it into the terminal |
| `codex` | ChatGPT (Codex) | Open a URL and enter a short code |
| `antigravity` | Google (Antigravity) | Open a URL and sign in with Google |
| `copilot` | GitHub Copilot | Open a URL and enter a short code |

```sh
novacode login claude
```

novacode prints the URL (and tries to open your browser with `xdg-open`). Then it waits for you. You can also run `/login claude` inside a session. Logins are stored in `~/.novacode/auth.json` and tokens refresh by themselves.

The first run picker (see [quickstart](quickstart.md)) starts these logins for you.

Inside a session, `/login <provider>` hands the terminal to `novacode login`. It has no `--account` form, so add extra accounts from the command line. The models of these providers come from the matching catalog entry (`claude` from `anthropic`, `codex` from `openai`, `antigravity` from `google`, `copilot` from `github-copilot`). Copilot login also prints `enabled models: …` after it turns on the Copilot models for your account.

### Logging in on a machine without a browser

Every login works over SSH. Open the printed URL on any device.

- **claude**: after you approve, the page shows a code. Paste it at the `code:` prompt. The full callback URL works too.
- **codex** and **copilot**: enter the printed code on the page. novacode keeps polling until you approve. Nothing to paste.
- **antigravity**: novacode listens on `localhost:51121` for the redirect. On a remote machine the browser's redirect to `localhost` will fail to load. That is fine: copy the full URL the browser ended on and paste it into the terminal.

### Several accounts

Add more accounts for one subscription with `--account`:

```sh
novacode login claude                    # account "default"
novacode login claude --account work
novacode login claude --account second
```

When an account runs out (quota used up, or a rate limit longer than novacode is willing to wait) before a reply starts, novacode switches to the next account of that provider and tells you in the transcript. It remembers when the limit resets, so a new session doesn't start on a used-up account. Order is `default` first, then by name.

novacode never falls back to a metered API key. If every account is used up, the error says when the limit resets and suggests adding an account or switching model with `/model`.

`--account` only works for subscriptions. API keys have one key per provider.

## API-key providers

Every other provider uses an API key. List them with:

```sh
novacode models --all
```

The columns are the provider id, its name, the main env var, and `connected` if novacode can already use it. Common ones:

| Provider | Env var |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `google` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` or `GOOGLE_GENERATIVE_AI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `mistral` | `MISTRAL_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `togetherai` | `TOGETHER_API_KEY` |

Some others have names you might not guess:

| Provider | Env var |
|---|---|
| `zai`, `zai-coding-plan`, `zhipuai` | `ZHIPU_API_KEY` |
| `alibaba` | `DASHSCOPE_API_KEY` |
| `moonshotai`, `moonshotai-cn` | `MOONSHOT_API_KEY` |
| `kimi-code-plan-global` | `KIMI_API_KEY` |
| `minimax`, `minimax-coding-plan` | `MINIMAX_API_KEY` |
| `huggingface` | `HF_TOKEN` |
| `friendli` | `FRIENDLI_TOKEN` |
| `fireworks-ai` | `FIREWORKS_API_KEY` |
| `ollama-cloud` | `OLLAMA_API_KEY` (address `https://ollama.com/v1`) |

About 55 providers are bundled, among them `cerebras`, `nvidia`, `perplexity`, `cohere`, `helicone`, `nebius`, `sambanova`, `scaleway`, `poe` and `opencode`. Most use the pattern `<NAME>_API_KEY`. The list above shows the rest.

### Give novacode a key

Pick one way. If several are set, novacode uses the first in this order:

1. `--api-key` on the command line (this run only; needs `-m provider/model` or a saved model)
2. A key saved with `novacode login <provider>`
3. `providers.<id>.apiKey` in settings
4. The provider's env var

```sh
export OPENROUTER_API_KEY=sk-or-...
novacode -m openrouter/anthropic/claude-sonnet-4.5

novacode login openrouter              # asks for the key without echo, checks it, saves it
novacode -m openai/gpt-5 --api-key sk-...
```

`novacode login <provider>` checks the key with a free model-list call before saving it. If the check fails it asks whether to save anyway.

### Keys in settings

`apiKey` in `~/.novacode/settings.json` takes a literal key, or an indirection so the secret stays out of the file:

| Value | Meaning |
|---|---|
| `"sk-..."` | The key itself |
| `"$MY_KEY_VAR"` | Read this environment variable each time |
| `"!pass show openrouter"` | Run this shell command each time; its output is the key |

```json
{
  "providers": {
    "openrouter": { "apiKey": "!pass show api/openrouter" }
  }
}
```

You can enter `$VAR` or `!command` at the `novacode login` prompt too. Whitespace and invisible characters in a pasted key are removed. See [config](config.md) for every setting.

## Choosing a model

| Where | How |
|---|---|
| Every run | `"model": "claude/claude-opus-5-5"` in settings |
| One run | `novacode -m openai/gpt-5` |
| In a session | `/model` (picker), `/model <id>`, or Ctrl+L |

A `:level` at the end sets the thinking level:

```sh
novacode -m claude/claude-opus-5-5:high
```

A trailing `:something` that is not a level stays part of the model id.

### Aliases

You can leave out the version. `claude/claude-opus` becomes the newest Claude Opus model novacode knows. The alias is fixed when the session starts. An alias matches a model family or the start of an id (`name-`).

### Listing models

```sh
novacode models              # models of connected providers
novacode models sonnet       # only ids containing "sonnet"
novacode models --all        # every provider, connected or not
novacode models --refresh    # update model info from models.dev first
```

Each row shows the model, its context size, whether it supports thinking and images, and the API price per million tokens (API-key providers only). novacode refreshes the model info in the background about once a day.

### Thinking levels

Levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The default is `medium` (setting `thinking`). Change it with `/thinking`, `/thinking <level>`, Shift+Tab, or the `:level` suffix. If a model doesn't support your level, novacode uses the nearest one it supports. Models without reasoning always run with `off`.

## Local models

Ollama and LM Studio are built in and need no key. If you set `OLLAMA_API_KEY` or `LMSTUDIO_API_KEY`, it is used:

| Provider | Default address |
|---|---|
| `ollama` | `http://127.0.0.1:11434/v1` |
| `lmstudio` | `http://127.0.0.1:1234/v1` |

```sh
ollama pull qwen3-coder
novacode -m ollama/qwen3-coder
```

If the server is not running, the provider is skipped quietly in `/model` and `novacode doctor`. For a server on another machine, change its address:

```json
{ "providers": { "ollama": { "baseUrl": "http://gpu-box:11434/v1" } } }
```

### Any OpenAI-compatible server

For llama.cpp, vLLM or any server with an OpenAI chat-completions API, add a provider:

```json
{
  "providers": {
    "vllm": {
      "api": "openai-chat",
      "baseUrl": "http://localhost:8000/v1",
      "keyOptional": true
    }
  }
}
```

```sh
novacode -m vllm/Qwen/Qwen3-Coder-30B-A3B-Instruct
```

| Field | Meaning |
|---|---|
| `api` | `openai-chat` for most servers. Also: `openai-responses`, `anthropic`, `gemini`, `process` |
| `baseUrl` | The server's `/v1` address |
| `keyOptional` | `true` if the server needs no key. Otherwise set `apiKey` or export a key |
| `apiKey` | Key, `$VAR` or `!command` |
| `headers` | Extra HTTP headers, an object of strings |
| `env` | A list of env var names to try for the key, in order, e.g. `["VLLM_KEY"]`. (Not for `process` providers.) |
| `compat` | Quirk flags for the wire format, see below. Read by `openai-chat` (and, with other flags, `openai-responses`). |
| `command`, `args` | For `process` providers only. See [Your own provider program](#your-own-provider-program). |

`compat` flags for `openai-chat` (all optional). `openai-responses` has only `noMaxTokens` (the endpoint rejects `max_output_tokens`) and `noEffortNone` (thinking `off` can't be sent as effort `none`).

| Flag | Meaning |
|---|---|
| `thinkingFormat` | How thinking is switched on: `openai` (default), `openrouter`, `deepseek`, `zai`, `qwen` or `none` |
| `maxTokensField` | `max_tokens` (default) or `max_completion_tokens` |
| `developerRole` | Send instructions as `developer` instead of `system` |
| `reasoningEffort` | `false` if the server rejects `reasoning_effort` (default true) |
| `reasoningContentOnAssistant` | Always send `reasoning_content` on assistant messages (DeepSeek, Kimi) |
| `thinkingAsText` | Send earlier thinking back as plain text |
| `toolResultName` | Put the tool `name` on tool results |
| `streamUsage` | Ask for usage in the stream (default true) |
| `cacheControl` | `anthropic`: add cache markers to content parts (OpenRouter to Claude or Gemini) |
| `midConvoSystem` | The server accepts `system` messages mid-conversation |
| `promptCacheKey` | Send the session id as `prompt_cache_key` |

A model novacode has no data for gets defaults: 128k context, 16k output, images and thinking on. Set real numbers per model:

```json
{
  "models": {
    "vllm/Qwen/Qwen3-Coder-30B-A3B-Instruct": { "contextWindow": 65536, "maxTokens": 8192, "images": false }
  }
}
```

Fields of a `models` entry (all optional): `contextWindow` (or `context`), `maxTokens` (or `output`), `images`, `reasoning`, `name` and `cost`. This works for models novacode already knows too.

Settings in `providers` also override built-in providers field by field, for example a different `baseUrl` or extra `headers` for `openai`.

## Your own provider program

A provider can be any program that speaks JSON-RPC on stdio. Set `"api": "process"` with a `command`. It needs no key because the program handles its own credentials. The protocol is in [extensions](extensions.md#custom-providers).

```json
{
  "providers": {
    "mine": { "api": "process", "command": "python3", "args": ["/path/to/provider.py"] }
  }
}
```

| Field | Meaning |
|---|---|
| `command` | Program to start. Required. |
| `args` | List of arguments. |

`env` is not supported for these providers yet. To set variables, wrap the command: `"command": "env", "args": ["MY_VAR=x", "python3", "/path/to/provider.py"]`.

Project settings only apply in folders you trust, so a repository can't start a provider program on its own.

## Check and remove

```sh
novacode auth status
```

Shows every login and key, where it comes from (login, settings, or env var), and whether a subscription token is valid. It reads local files only and makes no network calls.

```sh
novacode doctor
```

Checks each connected provider with a model-list call (free), and also checks your settings and key bindings.

```sh
novacode logout claude                  # every claude account
novacode logout claude --account work   # one account
novacode logout openrouter              # a saved API key
```

`logout` removes only what is in `auth.json`. Keys from env vars or settings are not touched.

## See also

- [Quickstart](quickstart.md)
- [Config](config.md)
- [Extensions](extensions.md)
- [Security](security.md)
- [Troubleshooting](troubleshooting.md)
