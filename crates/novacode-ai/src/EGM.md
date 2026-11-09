# novacode-ai (src)

## Overview

The provider layer (L2). It turns a `provider/model[:level]` choice into a ready
**adapter** that streams one model call as `StreamEvent`s. It holds the wire
adapters (Anthropic, OpenAI Chat, OpenAI Responses, Gemini, Copilot, process
providers, a scripted provider for tests and the hidden `faux/script` model), credentials and subscription logins,
model metadata, retry and schema-repair policy, cross-model history transform,
classifier models and request tracing.

## Interface

- `adapter.rs`: `Adapter` trait (`stream`, `warm_ttl`, `native_tool_changes`,
  `follow_account`, `accounts`, `switch_account`, `list_models`), `Request`
  (+ `messages_with_tail`),
  `CacheRetention`, `Builder` (the one shared block-to-event builder),
  `collect`, `tail_text`, `EVENT_BUFFER` (256).
- `connect.rs`: `Providers` (`load`, `resolve`, `adapter`, `adapter_for`,
  `allow`, `allow_id`, `profile`, `connected`, `key_source`, `api_key`),
  `Route` (`spec`, `clamp`, `thinking_change_keeps_cache`), `ConnectError`,
  `split_level`, `cache_path`.
- `catalog.rs`: `Catalog` (`parse`, `load`, `models`, `get`, `info`,
  `resolve_alias`), `ModelInfo`, `Profile`, `Api`, `Env`, `profiles`,
  `strip_date`, `effort_name`, `clamp_thinking`, `refresh`, `stale`,
  `MODELS_DEV_URL`; re-exports `novacode_types::Cost`.
- `auth.rs`: `AuthStore` (`new`, `put`, `put_key`, `stored_key`, `logins`,
  `accounts`, `select`, `credential`, `access_token`, `mark_exhausted`,
  `exhausted_until`, `remove`, `secrets`, `has`, `cached_access`), `OAuthCredential`,
  `AuthError`, `pkce`, `resolve_key`, `DEFAULT_ACCOUNT`; login flows in
  submodules `claude`, `codex`, `antigravity`, `copilot`.
- `rotate.rs`: `Live` (`new`, `account`), the subscription adapter that
  refreshes tokens and rotates across logged-in accounts.
- Wire adapters: `anthropic::Anthropic` (+ `with_native_search`,
  `with_headers`, `with_http`, `with_provider`, `with_base_url`, `Auth`,
  `OAuthProfile`, `PROVIDER`, `build_body`, `ThinkingStyle`,
  `thinking_style`, `clamp_thinking`, `native_tool_changes`,
  `in_conversation_effort`, `swap_search_tool`), `openai_chat::OpenAiChat`
  (+ `Compat`, `build_body`), `openai_responses::OpenAiResponses` (+ `Compat`,
  `Auth`, `build_body`), `gemini::Gemini` (+ `Auth`, `build_request`),
  `copilot::Copilot` (+ `PROVIDER`, `wire`, `Wire`), `process::Process` (+ `Config`,
  `API_VERSION`), `faux::Faux` / `FauxReply` (test double, and
  the hidden `faux/script` provider via `Faux::from_env`, served by
  `Providers::adapter`).
- `http.rs`: shared send-and-stream loop: `drive`, `Wire` trait, `channel`,
  `client`, `classify_openai`, `Failure`, `CONNECT_TIMEOUT` (30 s),
  `IDLE_TIMEOUT` (120 s), `STREAM_IDLE_TIMEOUT` (300 s).
- `retry.rs`: `should_retry`, `retry_after`, `delay`, `MAX_RETRIES` (2),
  `MAX_RETRY_DELAY` (60 s).
- `schema.rs`: `openapi_subset`, `Rejection`, `rejected`, `strip`, `Quirks` (`load`,
  `ephemeral`, `apply`, `learn`).
- `transform.rs`: `for_target`, `Target`, `safe_id`, `NO_RESULT`,
  `IMAGE_OMITTED`, `KEEP_IMAGES`, `OLD_IMAGE`.
- `classify.rs`: `classify`, `find`, `key_env`, `MODELS`, `Model`, `Wire`,
  `Question`, `Answer`, `Outcome`, `Context`, `BoolCriteria`, `Image`.
- `quota.rs`: `cached`, `supported`, `Quota`, `Window` (`resets_in`), `MAX_AGE` (5 min).
- `json.rs`: `parse_tool_args` (parse with repair).
- `sse.rs`: `SseParser`, `SseEvent`.
- `trace.rs`: `Trace` (`--trace` JSONL writer).

## Invariants

- `Adapter::stream` never fails: every failure arrives as a terminal
  `StreamEvent::Error` on the channel.
- Every adapter builds events through the single `adapter::Builder`; the event
  channel is bounded (`EVENT_BUFFER`), so a slow consumer back-pressures the adapter.
- Retries happen only before any content has been streamed.
- `transform::for_target` is pure and deterministic: the same history gives the
  same bytes so the cached prefix holds. Every tool call gets a result.
- `Request::tail` is sent after the last cache marker and never stored.
- Token refresh holds the `auth.json` file lock, re-reads after locking, and is
  never cancelled, so a rotated refresh token is never lost.
- `rotate::Live` never falls back to a metered API key; it only rotates to
  another logged-in account, and only before any output.
- Model metadata layers, later wins: bundled `data/models.json` and
  `data/profiles.json` (compiled in with `include_str!`), then the refreshed
  cache, then `models.<provider/id>` in settings.
- `schema::Quirks` is advisory: an unreadable quirks file means learning again,
  never a failure.
- Traces redact any header whose name contains `auth`, `key`, `token`,
  `secret` or `cookie`.

## Side Effects

- Network: HTTPS calls to provider APIs, OAuth/device-code endpoints, quota
  endpoints and `https://models.dev/api.json`.
- Disk: `<home>/auth.json` (0600, file-locked; symlinks resolved to target),
  `<home>/cache/models-dev.json`, `<home>/cache/schema-quirks.json`,
  `<home>/cache/usage/<provider>-<account>.json` (shared quota cache),
  `<home>/cache/usage/<provider>.json` (legacy copy of the last-used
  account), and `--trace` JSONL files.
- Reads the `faux/script` reply file: `$NOVACODE_FAUX_SCRIPT_DIR/<label>.jsonl`,
  else `$NOVACODE_FAUX_SCRIPT`.
- Processes: `process::Process` spawns a custom provider program once per
  session and speaks JSON-RPC 2.0 over its stdio.

## Verification

- Run: `cargo test -p novacode-ai`
