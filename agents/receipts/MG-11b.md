# Receipt: MG-11b — CUDA DLL lookup without manual workarounds

- Date: 2026-10-19, America/Los_Angeles.
- Author: Codex desktop on CraftML, acting as Codex-cloud under SPEC 23.
- Reviewer: Claude, pending; no self-review request.
- Repository / branch: erikerikerik/craft-ai-manager / ai/mg-11b-cuda-dll-lookup.
- Base: PR #10 head afb555fbd22f45b2c81b08b1b8c0d68f1a9ee870 (not yet merged when work began).
- Machine: Windows, rustc 1.99.0 (b940084d7 2026-09-28), RTX 4090, driver 616.64.
- Fresh CARGO_TARGET_DIR: target/agent-mg-11b, inside E:\CraftML\work\wt\mg-11b.
- Models: E:\CraftML\work\models; CUDA store: E:\CraftML\work\cuda.
- Before the first card run: 22,495 MiB free / 24,564 MiB total. Ollama `ps` listed no models; no Strata inference process was present. No inference process was killed or restarted.
- PR, CI, merge: PR to be opened after local gates; CI and Claude approval pending. Not merged.

## Cause and fix

Read ort 2.0.0-rc.14 src/ep/cuda.rs and src/util/mod.rs, and ort-sys build/dynamic_link.rs.
The aggregate CUDA preloader loads four CUDA and eight cuDNN names. It omits
`cudnn_engines_tensor_ir64_9.dll` and `cudnn_ext64_9.dll`, although CAIM's WANTED
already includes them. cuDNN's later request for its tensor-IR engine therefore
searched outside the already-loaded module set and failed, as recorded in PR #10.

CAIM now iterates every WANTED name in its existing order, passing an absolute path
to `ort::util::preload_dylib`. That safe API uses `ManuallyDrop` to retain the handle
for the process lifetime. The existing per-folder cache prevents repeated loading.
No direct libloading dependency, unsafe code, PATH mutation, AddDllDirectory,
SetDllDirectory, or SetDefaultDllDirectories was needed. **WANTED actually contains
16 names (1 + 3 + 2 + 10), not the nineteen stated in the prompt.** All 16 are loaded.

The executable's own folder must contain both provider DLLs before CUDA preload.
The folder-taking check reports the missing filename and folder. `cuda status`
uses the same check. Tests cover each missing provider and a directory masquerading
as a provider file. The card goldens prepare the two provider DLLs automatically
from the profile folder when ort-sys could not symlink them into `deps`.

`error_on_failure()` remains enabled, and actual CUDA placement is still proved
by runtime profiling. Both catalog entries now say CUDA supported; platform checks
still limit the pinned library installation to Windows x86_64.

## Local gates

All Cargo commands used the fresh target directory and cached dependencies (`--offline`).

| Command | Result |
|---|---|
| cargo fmt --all -- --check | PASS |
| cargo clippy --offline --all-targets -- -D warnings | PASS |
| cargo test --offline | PASS: 386 passed, 7 ignored, 0 failed across all suites |
| cargo test --offline --release --test golden -- --ignored --nocapture --test-threads=1 card | PASS: 3/3, 39.61 s on final code/catalog |
| cargo test --offline --release --test golden -- --ignored --nocapture --test-threads=1 --skip card | PASS: 3/3; final timings below |

Every non-comment guard row in agents/guards.txt requests `cargo test`; the full
test run above satisfies all eleven rows without repeating an identical suite.
The initial fresh-target card run also passed 3/3 in 41.25 s, with no manual DLL
copy and the inherited PATH untouched. The final run uses the same build directory
after the catalog update. Raw local logs: E:\CraftML\work\logs\mg-11b-{card,cpu,test,clippy}.txt.

## Timings (milliseconds)

The GPU comparison's CPU measurements are included below; the separate CPU golden
run supplies CPU warm timings. Model load time is separate from first inference.

| Model / fixture | CUDA load | CUDA first | CUDA warm | comparison CPU load | comparison CPU first |
|---|---:|---:|---:|---:|---:|
| BiRefNet / buoy | 2677 | 642 | 267 | 2648 | 5360 |
| BiRefNet / person | 2553 | 582 | 265 | 2632 | 5441 |
| LaMa | 6636 | 650 | 123 | 6767 | 1363 |

| Model / fixture | CPU load | CPU first | CPU warm |
|---|---:|---:|---:|
| BiRefNet / buoy | 2583 | 5439 | 4535 |
| BiRefNet / person | 2765 | 5358 | 4474 |
| LaMa | 6830 | 1381 | 1361 |

The final separate CPU golden run passed 3/3 in 35.04 s.

Buoy/person IoU: 0.9997 / 0.9996. Neither GPU matte differs from its CPU matte by
more than one level at any pixel. LaMa mean difference from the committed output:
0.166 levels per channel; 886 / 179,058 masked samples differ from CPU by more than
one level. All pixels outside the fill mask remain identical.

## Negative checks

In a separate process, CRAFT_AI_CUDA pointed at the empty folder
E:\CraftML\work\tmp\mg-11b-empty-cuda. Running the single LaMa card golden
returned an actual **cpu** result, `fallback_reason: backend_init_failed`, and:

> Craft AI Manager could not load E:\CraftML\work\tmp\mg-11b-empty-cuda\bin\cudart64_13.dll: LoadLibraryExW failed

The CUDA-only assertion deliberately failed (Cargo exit 101), proving it does not
count a CPU fallback as GPU success. A wrapper checked all three evidence fields
and passed. No installed folder was renamed, and the environment override existed
only in that shell. The subsequent normal card run passed 3/3.
Log: E:\CraftML\work\logs\mg-11b-fallback.txt.

Copied the newly built caim.exe alone to an isolated temporary folder and ran
`cuda status`. It reported:

> not ready: Craft AI Manager is missing onnxruntime_providers_shared.dll in E:\CraftML\work\tmp\mg-11b-provider-status.

Log: E:\CraftML\work\logs\mg-11b-provider-status.txt. No installed binary changed.

## Remaining

Claude independent review, CI on the PR head, and merge. No macOS or Linux GPU
support is claimed. No upstream messaging or PhotoCraft queue changes were made.
Strata can resume after this task's GPU runs finish.
