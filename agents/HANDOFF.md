# HANDOFF: Craft AI Manager

## 2026-10-17 (early): Codex-Strata (autopilot) - MG-12 review round 1 (one P1, two P2)

- Same branch `ai/mg-12-resume-launch` and **PR #7**; this round answers the independent review of `517c40b`. Receipt: `agents/receipts/MG-12.md`, section "Review round 1".
- **P1 - a `.part` longer than its note threw the whole download away** (`crates/caim/src/fetch.rs:451`). The length check was `written != Some(note.bytes)`, so any part with more bytes than the note counted was deleted with its note - which is the normal shape after a kill between the 8 MiB checkpoints, so most abrupt interruptions lost every byte. A longer part is now cut back to the last checkpoint (`truncate_to`, `crates/caim/src/fetch.rs:521`) and that prefix resumes; only a part *shorter* than its note (bytes the note claims are missing) is rejected. Two new tests, one per side.
- **P2 - the next run made you wait two minutes for bytes it had just saved** (`crates/caim/src/fetch.rs:433`). `fetch_into_place` now takes a `Resume` enum (`NoLock`/`FreshLock`/`TakenOver`, `crates/caim/src/fetch.rs:225`) and `InstallLock` remembers how it got the lock: a lock created by this install means nobody else can own the pair, so a leftover pair is adopted at any age; only a `claim_stale` takeover keeps the both-halves-2-minutes-old rule. `a_half_file_someone_is_writing_to_is_not_picked_up` was reworked onto the takeover path, `a_pair_the_last_try_left_behind_is_adopted_without_waiting` is new, and the two keep-the-bytes tests no longer age the pair before rerunning - that is the user-visible part.
- **P2 - two jobs finishing at the same moment could split a log line or drop one** (`crates/caim/src/activity.rs:77`). The JSON and its newline were two writes, and the trim was an unsynchronized read-and-rewrite. The append and trim now run under a process-wide `LOG_LOCK` (`crates/caim/src/activity.rs:41`) and each record goes out as one buffer; `jobs_finishing_at_the_same_time_each_keep_one_whole_line` has four threads finish five jobs each and asks for 20 whole lines. Writers in another process are still not serialized - one manager owns this file - which the code and the Known gaps now say out loud.
- **A guard run caught a flaky test of mine** (not a Codex finding): `crates/caim/tests/activity_log.rs:18` read `activity.jsonl` before it existed, because `finish` (`crates/caim/src/job.rs:550`) drops the queue lock before writing the line so the disk never holds up the queue. The reader now waits (`wait_for_lines`); the old `lines()` helper is gone.
- Gates after the fix: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 247 passed / 0 failed / 2 ignored (was 243, +4 new tests), guards 9 of 9 in 72.4 s (`E:\CraftML\work\logs\guards-20261016-2357.txt`; the earlier 9 of 9 run is `guards-20261016-2339.txt`, and the 8 of 9 run between them is the flaky test above).
- **Next:** round 2 of the independent review (the last one allowed by the protocol). If it is clean and CI is green on the new head, `gh pr ready 7` then `gh pr merge 7 --merge`; if it finds something, fix it and merge on the reviewer's approval. Then a PhotoCraft docs PR closing the MG-12 row (PR #22 already moved it to `review`).

## 2026-10-16 (late night): Codex-Strata (autopilot) - MG-12 restart-safe resume, `app.json`, `caim open`, activity log

- Branch `ai/mg-12-resume-launch` from `origin/main` `e8a8cda` (MG-09 `3aca441`, MG-07 `fb80254`, MG-08 `e8a8cda` all in the base); **PR #7** (draft) against `main`. Receipt: `agents/receipts/MG-12.md`. No new dependency: `Cargo.toml` and `Cargo.lock` are untouched, so the MG-07 `deny` job has nothing new to check.
- **Restart-safe resume (CAIM-03).** An install keeps a note beside its bytes - `<file>.<tag>.part` and `<file>.<tag>.part.json` (url, revision, expected SHA-256, bytes written), the note rewritten atomically every 8 MiB (`META_EVERY`, `crates/caim/src/fetch.rs:161`). The next install of that model re-hashes the bytes, asks for the rest with `Range: bytes=<n>-`, and continues on 206; a 200 means the server ignored the range and the download starts over; any other status is an error that leaves the pair for the next try. A note naming another revision or SHA-256, or a part whose length disagrees with its note, deletes both. The finished hash still has to match the catalog. Progress starts at the resumed offset and `models install` says how many bytes were already on disk.
- **One deviation from the amendment's file names, on purpose:** the pair carries the install's random tag (`<file>.<tag>.part`, `<file>.<tag>.part.json`) rather than the bare `<file>.part` amendment 18 names. Two installs of one model each write their own `.part` today (the C6 invariant: only the bytes an install hashed itself get renamed into place); a single shared `<file>.part` would let them overwrite each other's bytes and each other's note. `adopt_part` (`crates/caim/src/fetch.rs:400`) finds the old pair by scanning the model folder and renames it under the new tag. It only takes a pair whose **both** halves are 2 minutes stale, so a half-file another install is still writing is never taken. `remove_dead_parts` now keeps a stale `.part` that has a note (it is resumable) and clears only orphaned notes and note-less parts.
- **`app.json`** (`crates/caim/src/endpoint.rs:120`): `{"version": 1, "exe": "<absolute path>"}` next to `endpoint.json`, written the same atomic way on every start of `serve` and `open`, never deleted on exit. A failure to write it is a sentence on stderr, not a failed start.
- **`caim open`** (`crates/caim/src/main.rs:96`): `launch_decision` (`crates/caim/src/endpoint.rs:257`) reads the endpoint file, then asks `GET /v1/health` on that port with the published token (250 ms). Answering: prints "Craft AI Manager is already running on 127.0.0.1:<port>. Nothing was started." and exits 0 without binding. Not answering: serves exactly like `caim serve` until MG-13 gives it a window.
- **Activity log** (`crates/caim/src/activity.rs`): one JSON line per finished job in `activity.jsonl` beside the endpoint file - `time`, `task`, `model`, `version`, `device`, `device_requested`, `fallback_reason`, `elapsed_ms`, `state`; the fields MG-10/MG-11 will fill are `null` today. Newest 1,000 kept, rewritten past 1,100, 8 KiB per line, an 8 MiB tail read when trimming. `Runner` writes it in `finish`, `cancel` and `request_stop` (once per job, so a cancelled job has one line). A write that fails is a sentence on stderr and cannot change what a job reports.
- Gates: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 243 passed / 0 failed / 2 ignored (was 218), guards 9 of 9. `cargo deny check` cannot run on this PC (cargo-deny is not installed and crates.io is unreachable); nothing in the dependency tree changed, and CI's `deny` job is the machine check.
- Manual check with the built binary: a `serve --fake-backend` on port 59119, then `caim open` printed the running line, exited 0, and left `endpoint.json` alone; `app.json` held the absolute path of the exe; `caim open --fake-backend` is exit 2 with the plain sentence; `caim --help` lists `open`. Enter-to-stop only watches stdin when it is a terminal, so the clean-stop-and-`app.json`-survives case stays under `crates/caim/tests/endpoint_file.rs:47` rather than a hand run.
- **Next:** the independent review of the PR, then CI. The PhotoCraft `agents/TASKS.md` MG-12 row goes to `review` on `ai/mg-12-status` (that repo's PR #22), because MG-12's code lives in this repo.
## 2026-10-16 (midday): Codex-Strata (autopilot) - MG-08 clean shutdown on Ctrl+C

- Branch `ai/mg-08-shutdown` from `origin/main` `ab2969a`; **PR #6** (draft) against `main`. Receipt: `agents/receipts/MG-08.md`. MG-09 merged as `3aca441` mid-flight, so the branch sits on it and the four files MG-09 also touched were merged by hand (`main.rs` and `README.md` clean; `guards.txt` and `HANDOFF.md` one conflict each, both sides kept).
- One new dependency: `ctrlc` 3.5.2 with the `termination` feature (MIT / Apache-2.0, inside the MG-07 `deny.toml` allow list). `Cargo.lock` gains 7 packages; on Windows ctrlc reuses the `windows-sys` 0.61.2 the tree already had.
- `StopHandle::press()` in `crates/caim/src/server.rs` is the shutdown path the signal handler calls. The first press is the same stop Enter does - the flag goes up and `Runner::request_stop()` cancels the running and the queued jobs; a later press returns true and `main.rs` exits 1 from the signal thread. `Server::shutdown()` is unchanged, so `endpoint.json` is still removed only when it holds this start's token.
- Three tests call that function directly, no real signals: one press cancels the running and the queued job and the next two presses say leave now; the same path through a real server removes the endpoint file and survives being called twice; and (round 2) a press that arrives before the server exists still stops that start and leaves no endpoint file.
- Manual Windows check with `GenerateConsoleCtrlEvent`: one Ctrl+C gives exit 0 and `endpoint.json` gone; two Ctrl+C 0 ms apart give exit 1 with the file left behind; two at 250 ms arrive after the clean stop had finished, so exit 0 and the file gone. Two at 30 ms is the boundary case and flips run to run (0, 1, 0 on the round-2 tree).
- **Review round 2** (Codex, one P2 on `312e50d2`): the handler was installed after `Server::start` had already written `endpoint.json`, so a supervisor that stops the process the moment it finds the port got the operating system's default handling instead of the clean stop. `StopHandle::new()` now makes the stop before the start, `Server::start_with_stop` adopts it and `attach()` hands it the runner before the endpoint file is written, and `serve` installs the handler first. `Server::start` is the same call with a fresh handle, so nothing else changes.
- Gates: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 218 passed / 0 failed / 2 ignored, guards 8 of 8 in 62.6 s (`E:\CraftML\work\logs\guards-20261016-1056.txt`). Built `--offline` throughout: crates.io is unreachable from this PC and ctrlc with its dependencies was already in the cargo cache; the seven new lock entries read MIT / MIT+Apache-2.0 / Zlib from their cached manifests, all inside the MG-07 allow list, which the `deny` job on this branch now confirms.
- **Next:** a re-review of the round-2 commits, then CI. The PhotoCraft `agents/TASKS.md` rows for MG-07, MG-08 and MG-09 go on `ai/mg-07-08-09-status`, which can be pushed now that PhotoCraft PR #17 has merged as `0852599`.
## 2026-10-15 (night): Codex-Strata (autopilot) - MG-07 CAIM CI: licence gate and MSRV job

- Branch `ai/mg-07-ci` from `origin/main` `ab2969a`, commit `8589a04c`; **PR #5** (draft) against `main`. Two files: new `deny.toml` and two new jobs in `.github/workflows/ci.yml`. No product code, no dependency change, `Cargo.lock` untouched.
- `deny.toml` allows exactly the ten licences the prompt names (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unicode-DFS-2016, CDLA-Permissive-2.0) and denies the rest; `advisories` runs with an empty ignore list; duplicate versions only warn; `[sources]` is crates.io only. Offline pre-check over `cargo metadata --offline --all-features` (126 packages, SPDX `OR`/`AND` legs): **0 outside the list**, so nothing had to be escalated for a licence widening.
- `deny` job pins `EmbarkStudios/cargo-deny-action` to `3c6349835b2b7b196a839186cb8b78e02f7b5f25` (v2.1.1, checked against annotated tag object `c3bbe7e4e3f7baeee1a3dd9aec0a3b2aded580fb` on 2026-10-16, runs cargo-deny 0.20.2). `msrv` job installs Rust `1.95` (the `rust-version` in `Cargo.toml`) and runs `cargo build --all-targets` plus `cargo test` on ubuntu.
- `cargo-deny` cannot be installed on this PC in this session (no route to crates.io: `cargo install cargo-deny --locked` -> `schannel: SEC_E_NO_CREDENTIALS`), so the conditional `MG-07 | craft-ai-manager | cargo deny check` guard line was **not** added - it would fail on every machine without the tool. The licence gate lives in CI. Receipt: `agents/receipts/MG-07.md`.
- Gates here in `target/agent-mg-07`: `cargo fmt --check` clean, `cargo clippy --all-targets -- -D warnings` clean, `cargo test` 206 passed / 0 failed / 2 ignored. The six existing `agents/guards.txt` lines are unchanged and pass in this worktree: 6 of 6 in 56.2 s, `E:\CraftML\work\logs\guards-20261016-1018.txt`.
- Pushed through the GitHub Git Data API with `gh` (`E:\CraftML\work\.codex-tmp\ghpush\push-via-api.ps1`, local only, not committed): `git push` still cannot authenticate on this account because every Git credential helper dies in the sandbox, and this path never reads or passes a stored credential. CN-00's scoped token plus a working helper would remove the need for either. The token that was printed into a session log has been revoked and the replacement carries `workflow` scope, which is why `.github/workflows/ci.yml` pushed cleanly this time.
- **Next:** independent review of the PR, then the two new CI jobs. MSRV 1.95 has never been built anywhere before this run - if the `msrv` job fails, the prompt's rule is a small fix or a raised `rust-version`, recorded in the receipt. PhotoCraft's `MG-07` row goes in on branch `ai/mg-07-status` after PhotoCraft PR #17 merges, because both PRs touch `agents/TASKS.md`.
## 2026-10-16 (midday): Codex-Strata (autopilot) - MG-09 review round 4

- Two things came back from the round-3 push `f9a25a7d`. Both CAIM CI jobs failed at `Format`: that patch put the `store.rs` test module's `use super::{ ... }` list on one line, past rustfmt's width, and `main` wraps the same list over two. `cargo fmt` fixed it in `fec29fd7`, which is green; the lesson is that `cargo fmt --check` has to be the last thing run before a push, not one of the first.
- Codex-cloud's re-review of `f9a25a7` then found a P1 at `crates/caim/src/store.rs:209`: `CRAFT_AI_MODELS=/somewhere/models/` names the same folder spelled with a trailing separator, and on Unix a trailing separator asks the filesystem about a directory, so `symlink_metadata` answers about the folder a link called `models` points at rather than about the link. `is_link` said "not a link", `linked_step` missed the root, `models list` counted through it, and `models remove` could reach `remove_dir_all` through it - the round-1 fix undone by one character.
- Round 4: `store::without_trailing_separators` rebuilds a path from `Path::components`, which drops a trailing separator and folds doubled separators and `.` steps while leaving a root (`/`, `C:\`) and an empty path alone. `is_link` and the start of `folder_usage` ask about that shape, and `models_dir_in` normalises the folder the environment names, so everything built from it is clean and `models where` prints one spelling.
- Three tests come with it: the extended `folder_usage_refuses_a_link_handed_in_as_the_folder`, the new `a_models_folder_named_with_a_trailing_separator_is_still_checked_for_a_link` (both `#[cfg(unix)]`, because Windows does not show the bypass - `Get-Item <junction>\` still reports `Junction` here), and a cross-platform `models_dir_in(Some("elsewhere/"))` assertion. Checked by hand with junctions on this box: `models where` prints the folder without the separator, and a junction at the models folder and one at `<store>\lama` both refuse the count and the removal spelled with a trailing separator, with the bytes behind them intact (`E:\CraftML\work\.codex-tmp\mg09\smoke-link-r5.txt`).
- Gates after round 4, in this worktree: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 215 passed / 0 failed / 2 ignored, guards 7 of 7 in 53.8 s (`E:\CraftML\work\logs\guards-20261016-1010.txt`).
- **Next:** Codex-cloud's re-review of the round-4 commit and both CAIM CI jobs. This is the fourth review round on PR #4; SPEC v1.1 amendment 17 allows two, so every round past that is Erik's call, which he gave on 2026-10-16.
## 2026-10-16 (morning): Codex-Strata (autopilot) - MG-09 review round 3

- Codex-cloud reviewed the second commit `813d63ca` on PR #4 and found one more P2: the disk-use walk followed a link it was handed. `folder_usage` opened the folder with `std::fs::metadata`, which follows a link, and `read_dir` follows it too, so with a symlink or junction at `<models>\<id>` `models list` counted the tree on the other side of the link and printed its bytes - while the doc comment promised a link is never followed. `DirEntry::file_type` only stops links met after the walk starts.
- Round 3, on `ai/mg-09-models`: `store::is_link` (a `symlink_metadata` answer), `folder_usage` reading the folder it was handed with `symlink_metadata`, and one shared `models::linked_step` that names the first link in `<models folder>/<id>/<version>` - the models folder included - which both the removal and `models list` now use. `models list` prints `on disk: a link, not a folder` and `total on disk: a link, not a folder` instead of a count taken through a link; the removal sentence is unchanged.
- Three `#[cfg(unix)]` tests cover it (the ubuntu job runs them), and a junction on this Windows box was checked by hand: a plain store still counts its bytes, a junction at `<store>\lama` and a junction at the models folder both refuse to be counted, and `models remove` still refuses with the link named in both shapes with the files behind it intact (`E:\CraftML\work\.codex-tmp\mg09\smoke-link-r3.txt`).
- Gates after round 3: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 215 passed / 0 failed / 2 ignored here (the three new tests are unix-only), guards 7 of 7 in 56.9 s (`E:\CraftML\work\logs\guards-20261016-0954.txt`).
- **Next:** Codex-cloud's re-review of this commit and both CAIM CI jobs. Erik asked for this third round; SPEC v1.1 amendment 17's two-round limit means any further change on this branch is his call again.

## 2026-10-16 (early, later): Codex-Strata (autopilot) - MG-09 review round 1

- Codex-cloud reviewed `dfe2ac0` on PR #4 and asked for changes: one P1 and two P2s. All three are fixed on `ai/mg-09-models` in the commit after `2d589147`, and the re-review asked for covers that commit only. This is round 1 of the two SPEC v1.1 amendment 17 allows.
- The P1 is the one that mattered: a symlink or Windows junction at `<models>\<id>` passed the two-component `strip_prefix` belt while `remove_dir_all` followed the link, so `models remove` could delete files outside the models folder. `installed_folder` now asks `symlink_metadata` about the id folder and the version folder and refuses with the link named. A `#[cfg(unix)]` test covers it in the ubuntu CI job, and a junction made here with `New-Item -ItemType Junction` printed the refusal and left the files behind it on disk.
- The two P2s: `models where` now says `(the default data folder)` when `CRAFT_AI_MODELS` is unset, which is what the README and the first receipt already promised; and `folder_usage` stops at 16 folders deep or 4096 folders held, with `models list` printing `too many folders to count` and `models remove` refusing, instead of holding a stack of paths sized by whatever is on disk.
- Gates after the fixes, in this worktree: `cargo fmt --check` 0, `cargo clippy --all-targets -- -D warnings` 0, `cargo test` 215 passed / 0 failed / 2 ignored (three tests added). Smoke re-run against the new build: `models where`, `models list`, `models remove`, and a real `caim serve --fake-backend` on port 61819 refusing while it answers.
- The ubuntu CI job then caught a bug in the new unix test itself: it made the folder the link points at inside the models folder instead of outside it, so its own write failed (`187 passed; 1 failed`). Fixed in the second commit on the branch, which is the second and last round SPEC v1.1 amendment 17 allows. The fixed test was also compiled and run here once with `#[cfg(windows)]` plus `symlink_dir`: it reached the link call and failed only there, because this box cannot make a symlink without privilege.
- **Next:** Codex-cloud's re-review of the second commit, then both CAIM CI jobs, before PR #4 can merge. The PhotoCraft `agents/TASKS.md` row for MG-09 still waits for PhotoCraft PR #17, which edits that same file.

## 2026-10-16 (early): Codex-Strata (autopilot) - MG-09 models remove, disk use, where

- Branch `ai/mg-09-models` from `main` `ab2969a`, commit `dfe2ac0a`; **PR #4** (draft) against `main`. No new dependencies, no wire-protocol change.
- `caim models list` ends each model line with `on disk: <bytes> bytes` and adds `total on disk: <bytes> bytes in <files> files, models folder <root>`. `caim models where` prints the folder and who named it. `caim models remove <id>` deletes `<id>\<version>\` (model file, `verified.json`, `acknowledged.json`, any `.part` and `.lock`), drops `<id>` when that was its last version, asks no questions, and refuses while the endpoint file names a port that answers on loopback. It never touches anything outside `<models folder>\<id>\<version>`: `catalog::is_name` on both parts plus a two-component `strip_prefix`.
- Gates: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; `cargo test` 213 passed / 0 failed / 2 ignored (was 206). `agents/guards.txt` gained `MG-09 | craft-ai-manager | cargo test`; all seven CAIM lines pass in this worktree - 7 of 7 in 89 s, `E:\CraftML\work\logs\guards-20261016-0022.txt`.
- Live check rather than a fake: `caim serve --fake-backend` published port 63705, so `models remove lama` refused with exit 3 and left the file on disk; after the server was stopped the same command removed it with exit 0.
- Receipt: `agents/receipts/MG-09.md`. **Next:** an independent review of the PR. The PhotoCraft `agents/TASKS.md` row for MG-09 waits for PhotoCraft PR #17, which edits that same file.
- Found on the way: PC-07's `run-guards.ps1` had a one-line bug - `$repos = if (...) { @($Repo) }` comes back as a bare string, so `$repos[0]` was `c` and `-Repo craft-ai-manager -RepoPath <worktree>` silently guarded the wrong checkout. Fixed on `ai/pc-07-guards` with a tripwire; that is the third push on PR #17, past the two-round limit in SPEC 17, so Erik decides.

## 2026-10-15 (late): Codex-Strata (autopilot) - PC-06 Part A: a real golden photo

- Branch `ai/pc-06-golden-photo` from `main` `7dd46c8`, commit `44748aa`; **PR #3** against `main`. Fixtures and `golden.rs` only - no product code, no new dependencies.
- `crates/caim/tests/fixtures/` now holds Wikimedia Commons "Orange buoy in water.jpg" by W.carter, **CC0 1.0** (licence read from the file page and confirmed through the Commons `extmetadata` API), cropped to 3283x3283 and resized to 448x448 (288,217 bytes, inside the 768 px / 300 KB limits), a hand-made soft-edged mask over the buoy, its handle and its reflection, and a fresh `expected.png` from a LaMa run that was looked at and judged a good removal. `SOURCE.md` records the URL, author, licence, dates and the exact mask recipe; `golden.rs` reads the side length from one const.
- `cargo test --test golden -- --ignored --nocapture`: 1 passed in 10.3 s - "141018 pixels outside the mask are identical; inside, 0.000 levels a channel from the committed answer, and the masked area moved from 65.6 to 23.1 away from the background colour".
- Clean networked build for the run: deleted `target/agent-local`, left `ORT_LIB_PATH` unset so `ort` fetched its own onnxruntime, rebuilt, and started `caim serve` from that build for PhotoCraft's PC-06 end-to-end (first fill 9.3 s of provider time including the model load, second 2.2 s).
- Gates: `cargo test` 206 passed / 0 failed / 2 ignored over 8 binaries; `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean. `agents/guards.txt` unchanged (all six lines are `cargo test`, all pass).
- Review round 1 (Codex-cloud on `0184365`, 2026-10-16): one finding, P1 - the session had a handoff entry but no receipt, which `AGENTS.md`
  requires. `agents/receipts/PC-06.md` now carries it; no code changed.
- **Next:** the PR needs an independent review. On the PhotoCraft side PC-05 and PC-06 are `review`; Erik still owes the five-minute hands-on check (`photocraft/agents/receipts/PC-06.md`).

## 2026-10-15: Codex - BF-03 blocking-review follow-up

- Same branch and PR #2; new commit follows `ae04452`. Review only this follow-up delta.
- Allocation-order guards cap the actual resize/mask intermediates at 512 elements. Both were
  mutation-tested: forcing across-first trips a 102,400,000-element guard before allocation.
- Corrected the initial 128 B/pixel budget policy to 32 B/pixel + 32 MiB and atomically replaced
  each upload's body charge with its peak charge. Oversized standalone requests return 413;
  contention alone returns 503. A real 8000×8000 RGBA16 HTTP upload returned 202 idle, then 503 for
  the same request while the first job held the budget. No `max_pixels` or SPEC change.
- Growing-axis weights are computed on demand so the realistic estimate also covers panoramas.
- RFC multipart boundary padding, 1,024 retained-record cap, leftovers > Content-Length, and
  test-only private `take_png` all covered. Receipt below records final checks.
- Central PhotoCraft task/review notes are on docs-only PR #14; no PhotoCraft product edits.
- Keep PR #2 draft until the local reviewer approves this commit and its CI is green, then the
  user authorized marking it ready and merging through GitHub. Earlier receipt policy is historical.

## 2026-10-14: Codex - BF-03 resource limits and recovery (handoff to Claude)

- Branch: `ai/bf-03-caim-resource-recovery`, directly based on Claude's BF-01 tip
  `e7204675b0e7719743038c947062a166d91d0bae`. Not merged. Independent review pending.
- Owns Codex report findings 2/3/4/7/8/9: smaller resize/mask intermediates; aggregate image-memory
  admission and bounded record/ID queues; deadline/cancellable socket writes; shutdown cancellation;
  PNG restoration on a detected failed transfer; multipart boundary-prefix handling.
- Receipt: `agents/receipts/BF-03.md` lists exact policy choices, changed files, tests, and integration
  instructions. **Claude should review/integrate these fixes rather than reimplementing them.**
- Claude retains ownership of PhotoCraft BF-02 and its pending `/v1/tasks` fallback follow-up.
  No PhotoCraft or SPEC/task-status edits; BF-01 downloader, lock, backend, and receipt untouched.
- Windows: fmt/clippy clean; full suite 199 passed, one ignored; explicit real-model golden run
  passed too (0.000 levels/channel difference, 59,175 untouched pixels identical). No fixture edits.
- Important Windows detail: socket shutdown on a clone alone did not wake reads in the regression
  test; reads/writes now poll the shutdown signal with 100 ms socket waits.
- Default image-data reservation budget 2 GiB, working estimate 128 bytes/pixel + 32 MiB; excludes
  ORT internals. Review this policy. Successful writes still consume results; detected failures
  restore them. No new protocol acknowledgement, new dependencies, or automatic merges.
- Integrate BF-01 first, then BF-03. Review Linux CI and new lifetimes before merging. See receipt
  for the limitations of the existing square/hard-mask synthetic golden fixture.

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

## 2026-10-14: local agent (Codex, GPT-6, `--profile strata`) - MG-04

**State**
- Branch `ai/mg-04-lama`, from `origin/main` at `4e1b02e` (MG-02 review). Receipt: `agents/receipts/MG-04.md`. MG-05 is folded in, as its prompt says.
- Step 0 first, its own commit `bc99e43`: `research` is a denied word of its own in the licence gate, so "for research purposes only" is caught; the `research only` phrase it made redundant is gone.
  LaMa still passes only because of Erik's D2 override, and a test says so.
- `ort 2.0.0-rc.14` (MIT OR Apache-2.0) with default features, CPU execution provider only, `api-31` = ONNX Runtime 1.31. No `image` crate: the resize is hand-written in `lama.rs`. 48 new lock entries,
  licences in the receipt; `byteorder` (Unlicense OR MIT, MIT leg taken) and `webpki-root-certs` (CDLA-Permissive-2.0) are flagged.
- `src/lama.rs`: the runner. Pure functions for the maths - `mask_box`, `crop_window` (box + `max(32 px, 25% of the longer side)`, grown to a square, clamped), `axis_weights`/`resize_plane` (area down,
  bilinear up), `mask_plane` + `dilate` (4 px at 512 scale, nearest-neighbour first), `model_input` (0-1 from 255 or 65535), `fill_back`, `blend` (`out = in x (1 - m) + fill x m` with `m = mask / 255`,
  never the dilated mask, so `mask == 0` comes back bit for bit). `trait Session` + `Pipeline` (session in a `Mutex<Option<..>>`, loaded on the first job and kept) + `OrtSession`, whose `run` spawns a
  scoped watcher thread that calls `RunOptions::terminate()` when a `DELETE` arrives, so a running inference stops in flight. `ready_pipeline_in` builds a pipeline only when MG-02 says the model is
  installed (file + size + `verified.json`, no re-hash) and acknowledged.
- Backend selection: `ServeOptions` gained `models_root` (the test seam), and `caim serve` without `--fake-backend` now picks `Backend::Lama` / `Fake` / `NoModel` at start; `/v1/tasks`, the job refusal sentence
  and `caim serve`'s own "backend:" line all follow that choice. `--fake-backend` is byte-identical to MG-01.
- Stages (the MG-05 part): progress 0.0/0.2 prepare, 0.25 load model (first job only), 0.3/0.9 run, 0.9/1.0 finish, each with a `message` naming the stage; `cancel` is checked between every stage. Provenance
  is `lama` / `cpu` / `local` with real `elapsed_ms`; no new JSON fields.
- Tests: 138 lib (18 new in `lama.rs`, 4 in `job.rs`) + 4 `endpoint_file` + 17 `protocol` (2 new) = 159, plus `tests/golden.rs` which is `#[ignore]`d. `tests/support` gained `no_model` and `lama_store`.
- Golden test: `crates/caim/tests/fixtures/{input.png,mask.png,expected.png,SOURCE.md}`. **The three images are synthetic** - this agent had no network, so the Wikimedia photo could not be fetched.
  `SOURCE.md` says so loudly, gives the generation recipe, and lists the five steps to swap in a real CC0 photo. The gate itself passes: 59,175 pixels outside the mask identical, 0.000 levels a channel
  inside against `expected.png`, and the masked area moved from 73.0 to 4.9 levels from the surrounding background colour.
- Manual round trip, recorded in the receipt: `caim serve` (no `--fake-backend`) plus `photocraft-cli run before.png --cmd select.rect ... --cmd edit.generativeFill` from PhotoCraft `main` at `7321d059`.
  A "Generative Fill" layer with `ai.model = lama`; job 1 9,059 ms (8,701 ms with the model load), job 2 1,593 ms (1,239 ms); every sample outside the selection identical between before and after.
  Files in `E:\CraftML\work\mg04-roundtrip\`.
- ONNX Runtime linking: offline, `ort-sys` cannot download the prebuilt runtime, so this machine linked against a local ONNX Runtime 1.31 through `ORT_LIB_PATH` plus DLLs next to the executables
  (Windows ships a 1.17 `onnxruntime.dll` in `System32`, which otherwise wins the search order). None of that is committed; `Cargo.toml` is plain `ort = "2.0.0-rc.14"`. A clean networked `cargo build`
  still needs confirming.
- Docs: README gained "Running it with a real model" (install, the installed-and-acknowledged rule, the lazy session, the stage table, cancel, the 512 px limit, provenance); `docs/models/lama.md` gained
  "How CAIM runs it (MG-04)" with the eight rules.
- Guards: `MG-04 | craft-ai-manager | cargo test` added; all 4 lines pass. fmt, clippy `-D warnings`, `cargo test` and `cargo test --test golden -- --ignored` are clean.
- PhotoCraft fork: MG-04 and MG-05 set to `review` on branch `ai/mg-04-status` (only `agents/TASKS.md`).
- **Neither branch was pushed: this agent had no network.** `git status` is clean in both repos; Erik pushes and opens the two compare links in the receipt.

**Next**
- Erik: push `ai/mg-04-lama` and `ai/mg-04-status`, open both PRs, and confirm one clean `cargo build` with network so the `ort` prebuilt-runtime download is proven. Then the review.
- Replace the synthetic golden fixture with a real CC0 Wikimedia photo (steps in `crates/caim/tests/fixtures/SOURCE.md`). Until then the evaluation gate proves the plumbing, not LaMa on a photograph.
- MG-04 review follow-ups likely: tiling for holes wider than 512 px (CAIM-13), pinning model id + version + SHA-256 into the result and the layer (B16, needs a SPEC amendment 11 change, U04), and
  whether one worker thread is enough (CAIM-12).
- PC-06 (end-to-end on the 4090 against the real manager) is now unblocked: it needs MG-05, which this is.
- Still open from MG-01: no Ctrl+C handler, jobs in memory only, MSRV declared but untested in CI.



