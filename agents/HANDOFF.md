# HANDOFF: Craft AI Manager

## 2026-10-14: local agent (Codex, GPT-6, `--profile strata`) - MG-00

**State**
- Branch `ai/mg-00-scaffold`, from `origin/main` at `40ba42f` (Erik's "Initial commit", README stub only).
- Scaffold: root `Cargo.toml` (workspace, `resolver = "3"`, `[workspace.package]` version 0.1.0 / edition 2024 /
  `rust-version = "1.95"` / `license-file = "LICENSE.md"`, `[workspace.lints.rust] unsafe_code = "forbid"`),
  one crate `crates/caim` (package `craft-ai-manager`, binary `caim`, no dependencies), `clippy.toml` (the four
  test-only allowances, copied from PhotoCraft), `rustfmt.toml` (`max_width = 160`, `use_small_heuristics = "Max"`,
  same as PhotoCraft), `.gitignore`, `.github/workflows/ci.yml`, `README.md`, `AGENTS.md`, `agents/`.
- `caim --version` prints `caim 0.1.0` and exits 0; anything else prints "Craft AI Manager is not implemented yet:
  MG-01 builds the loopback server." and exits 2. `lib.rs` and `main.rs` both start with PhotoCraft's deny list
  (`#![forbid(unsafe_code)]` plus the six no-panic clippy lints).
- `LICENSE.md`: PolyForm Noncommercial 1.0.0, downloaded from
  `https://polyformproject.org/licenses/noncommercial/1.0.0.txt` (SHA-256 of the downloaded text
  `ffcca38841adb694b6f380647e15f17c446a4d1656fed51a1e2041d064c94cc8`), with
  `Required Notice: Copyright Erik Kristianson (https://github.com/erikerikerik)` as the first line. Not typed from
  memory.
- PhotoCraft fork: MG-00 set to `review` on branch `ai/mg-00-status` (one line in `agents/TASKS.md`).
- Receipt: `agents/receipts/MG-00.md`.

**Next**
- MG-01: the server (bind `127.0.0.1:0`, token per start, `endpoint.json` with user-only permissions,
  `CRAFT_AI_ENDPOINT` override, `/v1/health`, `/v1/tasks`, the jobs API and multipart caps from SPEC v1.1
  amendment 11). Replace `Action::NotImplemented` / `EXIT_NOT_IMPLEMENTED` then.
- MG-02: model store + manifest (SPEC amendment 14). Move the MG-03 result, `agents/models/lama.md` in the
  PhotoCraft fork, to `docs/models/lama.md` here.
- CI has no cache step yet (nothing to cache at zero dependencies); add `Swatinem/rust-cache` in MG-01 when the
  dependency tree appears, pinned to a full SHA like the other actions.

## 2026-10-14: local agent (Codex, GPT-6, `--profile strata`) - MG-01

**State**
- Branch `ai/mg-01-server`, from `origin/main` at `0d33534` (MG-00 merged). Receipt: `agents/receipts/MG-01.md`.
- `caim serve` (the default with no arguments) binds `127.0.0.1:0`, makes a 32-byte token per start, and writes
  `endpoint.json` atomically (temp file + rename) at amendment 11's path, with `CRAFT_AI_ENDPOINT` as the override;
  Enter stops it cleanly and removes the file. `--version` and `--help` still answer without starting anything.
- `crates/caim/src/`: `error` (the status set), `token` (64 hex, constant-time compare), `endpoint`, `cli`,
  `http` (httparse, 8 KiB header line, 64 headers, 1 GiB body cap checked before reading, `Content-Length` +
  `Connection: close` on every reply), `multipart` (the three parts, in order, and nothing else), `image`
  (png 0.18, 8/16-bit, 1/3/4 channels), `protocol` (the JSON shapes, `JobRequest::validate`, 64 MP),
  `job` (one worker, FIFO, queue 8, results 10 minutes or until read), `server` (16 connections, 30 s read
  timeout, token then Host then route), `backend` (the fake mean-colour fill, 10 progress steps, cancel between
  them). `main.rs` no longer has `Action::NotImplemented`.
- Dependencies: `serde`, `serde_json`, `png 0.18`, `httparse`, `getrandom`, `thiserror`. No async runtime, no web
  framework, no multipart crate.
- Tests: 64 lib + 15 `tests/protocol.rs` + 4 `tests/endpoint_file.rs` = 83, all over real TCP. Guards file now has
  the `MG-01 | craft-ai-manager | cargo test` line.
- Manual round trip, both recorded in the receipt: the server alone against `%LOCALAPPDATA%\CraftAI\endpoint.json`
  (3,600 samples of `result.png` compared against the contract's arithmetic, 0 mismatches; clean Enter shutdown
  removes the file; the next start replaces it), and PhotoCraft `photocraft-cli run … --cmd edit.generativeFill`
  against the running server, which made a "Generative Fill" layer the size of the selection filled with the mean
  colour and reported `ai.model` = `fake-fill`.
- Also from MG-00's review: `repository` and `homepage` are in `[workspace.package]`, and CI now caches with
  `Swatinem/rust-cache` pinned to `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` (v2.9.2, verified by cloning the tag).
- PhotoCraft fork: MG-01 set to `review` on branch `ai/mg-01-status` (one line in `agents/TASKS.md`).

**Next**
- MG-02: the model store and manifest (amendment 14), and move the MG-03 result, `agents/models/lama.md` in the
  PhotoCraft fork, to `docs/models/lama.md` here.
- MG-04: the `ort` ONNX backend and the LaMa runner (crop, 512x512, normalise, feather, blend inside the mask only).
  MG-05: staged job runner (download / prepare / run / finish) with cancel between stages.
- Two things MG-01 left open on purpose: no Ctrl+C handler (`std` has none and MG-01 added no dependency for one, so
  Enter is the clean stop and Ctrl+C leaves `endpoint.json` behind), and `rust-version = "1.95"` is declared but not
  tested - an MSRV job in CI is worth adding.
- `agents/FEATURES.md` row CAIM-01 in the PhotoCraft fork is what MG-01 implements; it is still `todo` for the review
  to tick.

## 2026-10-14: local agent (Codex, GPT-6, `--profile strata`) - MG-02

**State**
- Branch `ai/mg-02-models`, from `origin/main` at `2d01bfa` (MG-01 review). Receipt: `agents/receipts/MG-02.md`.
- Step 0 first, its own commit `1cc7443`: `endpoint::create_private` makes `endpoint.json` and its temp file with mode `0o600` on
  Linux and macOS (`OpenOptionsExt::mode` under `#[cfg(unix)]`), plus two `#[cfg(unix)]` tests that read the mode back. Windows is
  unchanged - `%LOCALAPPDATA%` already carries a per-user ACL.
- Catalog: `crates/caim/catalog/models.toml`, embedded with `include_str!`, one `[[model]]` block with every amendment 14 field
  plus `[model.override]`. The one entry is LaMa from the PhotoCraft note (now `docs/models/lama.md`), flagged for its Places365
  training data with Erik's D2 override and the user-facing note.
- `src/catalog.rs`: `parse` / `load` / `gate`. The gate returns every problem, not the first: missing, empty or `UNCLEAR` fields,
  a non-HTTPS URL, a `sha256` that is not 64 lowercase hex, a file that is not `.onnx` / `.safetensors`, a duplicated id + version,
  names that could escape the store folder, and a denylisted licence (gpl / lgpl / agpl / `nc` as a token / openrail / research-only /
  non-commercial) without a complete override. `the_embedded_catalog_passes_the_gate` is the CI tripwire.
- `src/store.rs`: `<data dir>/CraftAI/models/<id>/<version>/<file>`, `CRAFT_AI_MODELS` to move it; installed means file + size +
  `verified.json` naming the same hash, so startup never re-hashes 200 MB. `acknowledged.json` records id, version, the note SHA-256
  and the time.
- `src/fetch.rs`: the `Fetch` trait (tests hand over bytes, never a network), `HttpFetch` on `ureq` 3 with rustls, `https_only(true)`
  and redirects that can never go to plain HTTP. Streaming to `<file>.part` while hashing, 64 KiB at a time; a server length or byte
  count past the catalog size is refused; a hash or size mismatch deletes the `.part` and names expected against actual; success
  renames into place and writes `verified.json`. Already installed means no request at all. `verify` re-hashes and rewrites the record,
  and clears it when the file has changed.
- `src/models.rs` + `src/cli.rs` + `src/main.rs`: `caim models list`, `caim models install <id> [--accept-note]`,
  `caim models verify <id>`, plain sentences on every error, a redrawn percentage while downloading, and exit codes 0 / 1 / 2 / 3
  (3 = unknown model, not installed, or the note needs acknowledging) documented in `--help`.
- Dependencies: `toml 1.1.8`, `sha2 0.11.1`, `ureq 3.4.2` (rustls + ring, which built fine on MSVC). Licences are listed in the
  receipt; one is outside the allowed list - `webpki-roots 1.0.9` is CDLA-Permissive-2.0 - and is flagged there for Erik.
- Tests: 116 lib (114 on Windows, the two `#[cfg(unix)]` mode tests only run on Linux/macOS) + 4 `tests/endpoint_file.rs` +
  15 `tests/protocol.rs`. `src/testkit.rs` is `#[cfg(test)] pub(crate)`: a scratch folder with cleanup, a fixture catalog entry,
  and a `FakeFetch` that counts calls.
- Manual check, recorded in the receipt: `caim models install lama --accept-note` pulled the real 208,044,816 bytes in 5,140 ms and
  computed SHA-256 `1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6`, which is the catalog value and MG-03's
  measured value; the second run said "already installed ... Nothing was downloaded"; `caim models verify lama` re-hashed it in
  4,227 ms. Nothing was committed - the model lives in `%LOCALAPPDATA%\CraftAI\models\lama\1\`.
- Docs: `docs/models/lama.md` copied from PhotoCraft `agents/models/lama.md` with a line saying where it came from; the README now
  covers the models commands, the model paths and `CRAFT_AI_MODELS`, the gate, the acknowledgment and the exit codes. `.gitignore`
  needed `!docs/models/` because the `models/` weight rule also matched that folder.
- PhotoCraft fork: MG-02 set to `review` on branch `ai/mg-02-status` (one line in `agents/TASKS.md`). CAIM-03 and CAIM-11 in
  `agents/FEATURES.md` are left `todo` for the review to tick, as with CAIM-01.

**Next**
- MG-04: the `ort` ONNX backend and the LaMa runner (crop the mask box plus padding, 512x512, normalise per `docs/models/lama.md`,
  resize back, feather, blend only inside the mask), and the golden test. MG-05: the staged job runner with cancel between stages.
- Where MG-02 leaves the seam: `/v1/tasks` still answers from `--fake-backend` only. Once MG-04 exists, the task list should come
  from `catalog::load()` filtered by `store::is_installed`, and a job should pin the model id, version and SHA-256 into its result
  (amendment 14, B16).
- For the review: the `webpki-roots` licence question, and whether `acknowledged.json` should be one record per model folder
  (today) or shared so a re-download does not re-ask.
- Still open from MG-01: no Ctrl+C handler, jobs in memory only, MSRV declared but untested in CI.
