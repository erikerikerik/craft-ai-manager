# AGENTS.md: guide for agents working on Craft AI Manager

Short guide. The long form lives in PhotoCraft, which owns the plan.

- **The contract lives in the PhotoCraft fork:** `erikerikerik/photocraft`, file `agents/SPEC.md`. Read v1.1
  amendments 11 (job protocol v1, wire details), 13 (candidate models and the training-data licence rule) and
  14 (CAIM runtime patterns: model manifest + licence gate, acknowledgment for flagged models, model pinned in
  results, tiled runner, priority lanes, evaluation gate). Do not copy the spec here: that file is the only
  source of truth, and a protocol change needs a SPEC edit first.
- **The task list is that repo's `agents/TASKS.md`** (the `MG-*` rows), with `agents/FEATURES.md` §4 for the
  CAIM feature checklist and `agents/prompts/local-mg-*.md` for the per-task prompts. Set the MG row's status
  there, not in this repo.
- **The never-crash rules apply** (PhotoCraft `AGENTS.md` §3, which CAIM adopts): no `unwrap()`, `expect()`,
  `panic!`, `unreachable!`, `todo!` or `unimplemented!` in non-test code - return an error and propagate with
  `?`; no `unsafe` (the workspace sets `unsafe_code = "forbid"`); `get()` rather than `[i]` or `[a..b]` for
  indices that come from input; `checked_*` / `saturating_*` for lengths and offsets; cap allocations sized by
  input; bound recursion; handle lock poisoning. `clippy.toml` allows `unwrap` / `expect` / `panic` / indexing
  in tests only, and CI runs `cargo clippy --all-targets -- -D warnings`.
- **Pull requests target `erikerikerik/craft-ai-manager` `main`.** One branch per task (`ai/<task>`), never
  push to `main`, and never use the `pull/new/<branch>` link Git prints.
- **Never read stored credentials** (`git credential fill`, Windows Credential Manager, browser stores, token
  files) and never pass them to other tools. API keys belong in the Windows Credential Manager / macOS Keychain
  (CAIM-08), not in a repo or a log.
- **Never commit model weights:** `.onnx`, `.safetensors`, `.ckpt`, `.pt`, `.pth`, `.bin` and `models/` are
  gitignored. ONNX and safetensors only, never pickle; downloads come from catalog URLs and are SHA-256 checked.
- **Before a PR:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and every
  command in `agents/guards.txt`. Use your own build folder (`CARGO_TARGET_DIR=target/agent-<name>`) and edit
  only files you own.
- **Write it down:** a receipt in `agents/receipts/<ID>.md` (template in that folder) and an update to
  `agents/HANDOFF.md` at the end of every session.
