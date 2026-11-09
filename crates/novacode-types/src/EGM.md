# novacode-types/src

## Overview
Plain data types shared by every novacode crate (layer L1). Everything lives in `lib.rs`. The crate does no I/O and holds no logic beyond small helpers. It defines the conversation model (messages, blocks, tool calls), the provider stream and agent event protocol, the extension UI protocol, token estimates, and the `ask_user` / `say` structured forms.

## Interface
- `Replay { provider, data }`: opaque provider data (thinking signatures, encrypted reasoning) kept byte-exact and sent back only to `provider`.
- `ToolCall { id, name, arguments, arguments_error, replay }`: `arguments` is `{}` when the model sent invalid JSON; the raw text then sits in `arguments_error`.
- `Block`: `Text`, `Thinking`, `Image { mime_type, data (base64) }`, `ToolCall`, `ProviderNative { provider, item }`, `Unknown`. Helper `Block::text`.
- `ToolDef`: tool definition sent to providers (`schema` is a plain `serde_json::Value`; `schemars::JsonSchema` is derived only on `Question` and `Choice`).
- `ErrorKind` with `is_retryable()`; `StopReason`.
- `Usage` with `cost(&Cost)` and `prompt_total()`; `Cost` (per-token prices).
- `ThinkingLevel` with `ALL` (7 levels), `as_str()`, `parse()`.
- `AssistantMessage` (`model`, blocks, usage, ...) with `text()` and `tool_calls()`.
- `Message` (tagged by `"role"`): `System`, `User`, `Assistant`, `ToolResult { call_id, tool_name, content, is_error, details }`, `Custom { kind, data }`; helper `Message::user(text)`.
- `typed_prompt(&str) -> Cow<str>`: shows an inlined skill (`<skill name="x">…</skill>`) as `/x` plus the user's own text.
- `BlockKind`, `StreamEvent` (provider streaming events), `AgentEvent` (session events the UI and extensions consume).
- `mod ui`: extension UI protocol — `API_VERSION = 1`, `Seg`, `Line`, `Time`, `Node`, `UiOp` (with `slot(ext)`).
- `mod estimate`: token estimates — `IMAGE_TOKENS = 1372`, `text()`, `message()`, `context()`.
- Structured user interaction: `Question`, `Choice`, `Reply` (`ask_user`), and `Say`, `SaySection`, `SayQuestion`, `SayBlocker` (with `SayQuestion::paragraphs()`).

## Invariants
- `ToolResult.details` is UI-only and never sent to a model; `Custom` messages are harness-only and converted or dropped before sending.
- Serde policy (L1.6): data enums are tagged with `"type"` (snake_case), except `Message` (`"role"`), `StreamEvent` (`"stream"`), `ui::UiOp` (`"op"`) and `ui::Line` (untagged); `ThinkingLevel` (lowercase) and `BlockKind` (snake_case) are plain strings. Optional fields default and are skipped when empty; unknown fields are ignored; unknown blocks load as `Block::Unknown`.
- Maps keep insertion order (`serde_json` `preserve_order`), so stored bytes equal sent bytes (L1.7 rule 3). Never reorder or re-serialise `Replay.data` / `ProviderNative.item`.
- `Replay` and `ProviderNative` data go back only to the provider named in them.
- No I/O and no dependencies on other novacode crates: this is the bottom of the crate graph.

## Side Effects
- None. Pure data and pure helper functions.

## Verification
- Run: `cargo test -p novacode-types`
