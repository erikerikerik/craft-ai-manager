# Attribution

What Craft AI Manager builds on, and what each part is licensed under. The machine check is
`deny.toml` plus the `deny` job in `.github/workflows/ci.yml`, which allows only permissive licences
(MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, Unicode-DFS-2016, CDLA-Permissive-2.0) for
every crate in `Cargo.lock`. This file is about the things that check cannot see: other people's
code we read or adapted, the models themselves, and the photos in the tests.

CAIM's own licence is PolyForm Noncommercial 1.0.0 (`LICENSE.md`). Where a part below is more
permissive than that, the more permissive terms let you use that part on its own.

## Prior art we reused: upstream PhotoCraft pull requests

Upstream [PhotoCraft](https://github.com/erikerikerik/photocraft) is **MIT OR Apache-2.0**, so its
code can be reused in this repo under either licence. Two open upstream PRs reached the same model and
the same maths as MG-10 before we wrote ours, and both were read before and during the work:

| PR | What it does | What we took |
|---|---|---|
| [#2995](https://github.com/erikerikerik/photocraft/pull/2995) | Select Subject "High quality": BiRefNet-lite through `tract`, in a new `photocraft-ml` crate. | The checkpoint choice and its pin (`onnx-community/BiRefNet_lite-ONNX`, `onnx/model.onnx`), and an independent confirmation of the SHA-256 RS-01 measured. |
| [#4812](https://github.com/erikerikerik/photocraft/pull/4812) | Remove Background through ONNX Runtime, with OpenVINO, DirectML and CPU fallback. | The preprocessing contract (bilinear stretch to 1024x1024, RGB, /255, ImageNet mean and std, NCHW) and the finding that this graph's logits collapse on the DirectML execution provider - which is why `crates/caim/catalog/models.toml` marks `cuda` and `coreml` `untested` and only `cpu` `supported`. MG-11 also read its execution-provider fallback chain as a reference; CAIM's resolver follows amendment 18 instead, and nothing is taken from that PR. |

Neither PR has merged upstream, and neither is a dependency of this repo: nothing is `use`d from
them. What we took is the arithmetic and the facts about the graph, written here and named again in
the header of `crates/caim/src/birefnet.rs`. SPEC amendment 18 asks for exactly this credit line.

## The models

CAIM ships no weights. The catalog (`crates/caim/catalog/models.toml`) names where each file comes
from, and `caim models install` downloads it. Both models are flagged on their **training data**, so
each carries a signed `[model.override]` from Erik and needs one `--accept-note`.

| Model | Code | Weights | Training data | Override |
|---|---|---|---|---|
| `lama` - `lama_fp32.onnx`, 208,044,816 bytes | Apache-2.0 | Apache-2.0 | Places365: non-commercial research | D2 (Oct 13, 2026), personal use only |
| `birefnet-lite` - `birefnet_lite.onnx`, 224,005,088 bytes | MIT | MIT | DIS5K: research-only, commercial use prohibited; DUTS, HRSOD, UHRSD, HRS10K: no published licence | D11/D12 (Oct 17, 2026), non-commercial use only |

- **LaMa**: [advimman/lama](https://github.com/advimman/lama) (Apache-2.0), run from the ONNX port
  [Carve/LaMa-ONNX](https://huggingface.co/Carve/LaMa-ONNX). Facts and licence reading in
  `docs/models/lama.md`.
- **BiRefNet**: [ZhengPeng7/BiRefNet](https://github.com/ZhengPeng7/BiRefNet) (MIT, Copyright (c)
  2024 ZhengPeng7), weights published MIT at
  [ZhengPeng7/BiRefNet_lite](https://huggingface.co/ZhengPeng7/BiRefNet_lite); the file CAIM
  downloads is the ONNX export at
  [onnx-community/BiRefNet_lite-ONNX](https://huggingface.co/onnx-community/BiRefNet_lite-ONNX),
  byte for byte the GitHub release asset `BiRefNet-general-bb_swin_v1_tiny-epoch_232.onnx`. The
  preprocessing and the licence reading came from task RS-01 in the PhotoCraft fork
  (`agents/models/birefnet.md`); the dataset terms were read from each dataset's own page there.
  DIS5K's terms: <https://xuebinqin/DIS/DIS5K-Dataset-Terms-of-Use.pdf>.

## Runtime and libraries

- **ONNX Runtime**, prebuilt binaries pulled in by the `ort` / `ort-sys` crates - MIT. Since MG-11 that build has the
  CUDA execution provider compiled in, but CAIM registers CPU and CUDA only, and the NVIDIA libraries a CUDA run needs
  are a download the user asks for (next section). BiRefNet's session is still created with the CPU memory arena
  switched off (decision D12).
- Every other crate - `ctrlc`, `flate2` and `libloading` (both new in MG-11), `getrandom`, `httparse`,
  `png` (the PNG codec CAIM writes its own framing around), `serde`, `serde_json`, `sha2`, `thiserror`,
  `toml`, `ureq` - is in the `deny.toml` allow list, and `cargo deny check` fails the build if that changes.

## The NVIDIA runtime libraries (MG-11)

CAIM ships no CUDA and no cuDNN, and nothing NVIDIA is linked at build time. A CUDA run needs the NVIDIA DLLs, so
`caim cuda install --accept-note` downloads four archives pinned by name, version, size and SHA-256 in
`crates/caim/src/cuda.rs` from NVIDIA's own redist CDN, and unpacks from each archive only the library files CAIM names:

| Archive | Version | Licence |
|---|---|---|
| `cuda_cudart` | 13.4.92 | NVIDIA CUDA Toolkit (EULA) |
| `libcublas` | 13.8.0.4 | NVIDIA CUDA Toolkit (EULA) |
| `libcufft` | 12.4.0.43 | NVIDIA CUDA Toolkit (EULA) |
| `cudnn` | 9.27.0.42 (the `cuda13` build) | NVIDIA cuDNN Software License Agreement |

Both are EULAs rather than permissive licences, so they are deliberately outside the `deny.toml` allow list: that list
weighs the crates CAIM links, not files the user asks for on their own machine. What they do grant is what CAIM does with
them - NVIDIA's redist pages cover redistributing these components with an application. Because the licence is not
permissive, nothing is fetched quietly: the command names both agreements and the exact archives and their sizes, and
downloads only once the user has acknowledged them, which is the same gate MG-02 built for a flagged model's note and the
same `acknowledged.json` record.

The two crates MG-11 adds are permissive and inside the allow list: `libloading` 0.9.0 (ISC, pulled in by `ort`'s
`preload-dylibs` feature) and `flate2` 1.1.10 (MIT OR Apache-2.0), which inflates the archive entries CAIM needs.

The CUDA and cuDNN versions are pinned to what the ONNX Runtime build `ort-sys` fetches expects (`cuda13`), not chosen
freely. The sizes and SHA-256 values above were read from NVIDIA's own index files,
`https://developer.download.nvidia.com/compute/cuda/redist/redistrib_13.4.3.json` and
`https://developer.download.nvidia.com/compute/cudnn/redist/redistrib_9.27.0.json`, on 2026-10-18.

## The photos in the tests

Both golden-test photos are **CC0 1.0 Universal**, so the committed fixtures carry no attribution
obligation. `crates/caim/tests/fixtures/SOURCE.md` records the file page, author, licence reading and
the exact resize for each. The models that produced the expected answers are not CC0, and their files
are never committed.
