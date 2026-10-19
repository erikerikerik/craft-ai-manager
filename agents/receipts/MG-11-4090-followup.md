# MG-11: 4090 verification, 2026-10-19

Codex investigated Erik's failed card goldens in the existing MG-11 checkout.
GPU: NVIDIA GeForce RTX 4090, driver 616.64, over 22 GiB free.

## Findings and local changes

- ONNX Runtime provider registration suppressed its startup error. Added
  `error_on_failure()` so CAIM's existing CPU fallback receives the real error.
- The test executable lives in `release/deps`, but the build copies provider DLLs
  to `release`. The exposed error named the missing
  `release/deps/onnxruntime_providers_shared.dll`.
- After placing provider DLLs beside the test, cuDNN could not locate
  `cudnn_engines_tensor_ir64_9.dll`, although that file was installed. Adding the
  CUDA bin folder to this test process's PATH resolved the lookup.
- The card golden constructed a decision without a device name or card number,
  then asserted an NVIDIA device name. It now populates both from Machine::probe.

## Successful run

Build directory: `target/agent-cuda-diagnose`, release, offline.
`CRAFT_AI_MODELS=E:\CraftML\work\models`;
`CRAFT_AI_CUDA=E:\CraftML\work\cuda`.
Copied `release/onnxruntime_providers_*.dll` to `release/deps` and prepended
`E:\CraftML\work\cuda\bin` to the child process PATH (no permanent PATH change).
Command: `cargo test --offline --release --test golden -- --ignored --nocapture --test-threads=1 card`.
Result: **3 passed, 0 failed**, 39.34 s. Actual CUDA placement and accuracy asserted.

| Model / fixture | CUDA load ms | CUDA first ms | CUDA warm ms | CPU load ms | CPU run ms |
|---|---:|---:|---:|---:|---:|
| BiRefNet / buoy | 2840 | 618 | 287 | 2649 | 5538 |
| BiRefNet / person | 2573 | 437 | 286 | 2665 | 5460 |
| LaMa | 6424 | 604 | 123 | 6559 | 1281 |

Buoy IoU 0.9997, person IoU 0.9996. Neither matte had pixels differing from CPU
by over one level. LaMa mean difference from committed output: 0.166 levels per
channel; 886 of 179058 masked samples differed from CPU by over one level.

`cargo fmt --all -- --check` and `git diff --check`: pass.

## Still required

Fix provider DLL placement for Cargo tests and cuDNN's dependent-library lookup
in the normal product startup path. These results use the explicit workaround;
the catalog and advertised devices are unchanged. No commit, push, or merge.
