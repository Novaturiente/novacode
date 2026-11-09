# extensions/image-gen

## Overview
The `novacode-image-gen` crate: a standalone novacode process extension that adds the `generate_image` tool (Gemini image models). It has its own empty `[workspace]`, so it is not part of novacode's workspace or distribution and is built and installed on its own. It depends on `crates/novacode-ai` (path dependency) for the Antigravity login and the shared HTTP client.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - JSON-RPC stdio loop plus the `generate_image` tool; saves images under `<cwd>/.novacode/images/` and returns only their paths.
