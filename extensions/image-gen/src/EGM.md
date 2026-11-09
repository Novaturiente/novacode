# novacode-image-gen (src)

## Overview
Source of the `novacode-image-gen` process extension (see `docs/extensions.md`). It adds one tool, `generate_image`, which draws or edits images with Gemini image models. It talks to novacode over line-delimited JSON-RPC 2.0 on stdin/stdout. Each request is answered from its own tokio task, so several images can be generated at the same time.

- `main.rs`: the JSON-RPC loop. It handles `initialize`, `tool` (only `name == "generate_image"`) and `shutdown`.
- `image.rs`: the tool itself (L2.26): request body, model fallback, image parsing and saving.

## Interface
- JSON-RPC `initialize` (params `home`, `settings.apiKey`) → `{ "tools": [declaration()] }`. Builds the `GenerateImage` once (stored in a `OnceLock`).
- JSON-RPC `tool` with `params.name == "generate_image"`, `params.args`, `params.cwd` → `{ content, isError?, details: { images } }`. Before `initialize` it answers `{ "content": "not initialized", "isError": true }`.
- JSON-RPC `shutdown` → the process exits its read loop. Messages without an `id` (notifications) and unparseable lines are ignored; unknown methods answer `null`.
- `pub struct GenerateImage { pub auth: AuthStore, pub key: Option<String> }` (`image.rs`).
- `pub fn declaration() -> Value`: the tool schema. Args: `prompt` (required), `aspect_ratio`, `n`, `inputs` (image paths to edit).
- `pub async fn GenerateImage::run(&self, args: &Value, cwd: &Path) -> Value`: one tool call.

## Invariants
- Auth order: the Antigravity login (`novacode_ai::auth`, provider `antigravity`, `DEFAULT_ACCOUNT`, needs a `project`) first, trying `MODELS` in order: `gemini-3.1-flash-image`, `gemini-3-pro-image`, `gemini-3-pro-image-preview`. Then a Gemini API key with `gemini-2.5-flash-image`.
- API key source order: settings `apiKey`, then `GEMINI_API_KEY`, then `GOOGLE_API_KEY`.
- Home folder: `NOVACODE_HOME` if set, else `<params.home>/.novacode`; the login is read from `<home>/auth.json`.
- `aspect_ratio` must be one of the 10 values in `RATIOS` (default `1:1`); anything else is an error result. A missing `prompt` (`prompt is required`) or an unreadable `inputs` file (`<path>: <error>`) is also an error result, returned before any request is sent.
- `n` is clamped to 1–4. The `n` requests run in parallel (`join_all`).
- Only file paths are returned to the model, never image bytes, so images cost no tokens until the model `read`s one.
- File names are `<unix-secs>-<slug>-<i>.<ext>`; `slug` keeps the first 6 lowercase alphanumeric words (or `image`). Extension comes from the MIME type: `jpg`, `webp`, else `png`.
- Input images are read relative to `cwd`; their MIME type is sniffed from magic bytes (JPEG, GIF, WebP, else PNG).
- Once requests are sent, the result is an error only when no image was saved; partial failures are listed under `Failed:`.

## Side Effects
- Network: HTTPS POST to `{antigravity::ENDPOINT}/v1internal:generateContent` (bearer token, `user-agent: antigravity::USER_AGENT`, body wrapped as `{project, model, request, requestType: "image_gen", userAgent: "antigravity", requestId}`) or `https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent` (`x-goog-api-key`), 180 s timeout per request.
- Reads and may refresh the login in `<home>/auth.json` through `AuthStore`.
- Disk: creates `<cwd>/.novacode/images/` and writes image files there; reads `inputs` files.
- Writes JSON-RPC answers to stdout.

## Verification
- Run: `cargo test --manifest-path extensions/image-gen/Cargo.toml`
