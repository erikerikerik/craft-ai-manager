> Copied from `agents/models/lama.md` in the PhotoCraft fork (`erikerikerik/photocraft`) on Oct 14, 2026, when MG-02
> built the model catalog. The MG-03 row in that repo says this file "moves to the CAIM repo once MG-00 exists"; the
> original stays in PhotoCraft for now, so treat this copy as the CAIM-side record and keep the two in step if the
> facts change. `crates/caim/catalog/models.toml` cites this file as LaMa's provenance.

# LaMa ONNX: verified facts (MG-03)

Measured by the local agent (Codex + Strata) on the CraftML PC, Oct 13, 2026; written up by Claude from `report.txt`. Script: `check_lama.py` (ONNX only, no pickle).

## File

| | |
|---|---|
| Source | `https://huggingface.co/Carve/LaMa-ONNX/resolve/a3ee2fca54baebec351b8fa7786154ffa7555aa6/lama_fp32.onnx` |
| Revision | `a3ee2fca54baebec351b8fa7786154ffa7555aa6` |
| Size | 208,044,816 bytes |
| SHA-256 | `1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6` (**matches** the expected value) |
| Format | ONNX, opset 17, IR 8, exported from PyTorch 2.3.0, 17,480 nodes, `onnx.checker` OK |

## Tensors

| | Name | Shape | Type | Values |
|---|---|---|---|---|
| Input | `image` | `[batch, 3, 512, 512]` | float32 | RGB, **0–1** |
| Input | `mask` | `[batch, 1, 512, 512]` | float32 | **1 = fill (hole), 0 = keep**; values 0 and 1 |
| Output | `output` | `[batch, 3, 512, 512]` | float32 | RGB, **0–255** (clip to 0–255, then divide by 255) |

- The input size is fixed at 512×512, so the runner must crop, resize to 512, run and resize back (SPEC M1 item 6).
- Outside the hole the output matches the input (mean difference about 1e-8), so the model already composites. The runner should still blend only inside the feathered mask, to avoid resize blur outside it.
- The other value conventions tested (0–255 input, 0/255 mask, inverted mask) all gave wrong or damaged output; see `report.txt`.

## Speed (CPU, Ryzen 9 7950X, onnxruntime 1.31.0)

5 runs at 512×512: median **1,217 ms** (min 1,196, max 1,265).

## Test image

`input.png` (gradient with shapes), `mask.png` (centre square), `output.png`. The fill continues the gradient cleanly, but there was no object inside the hole, so this checks the wiring, not removal quality. MG-04's golden test should use a public-domain photo with a real object to remove.

## Licence note

Tagged Apache-2.0; port of big-lama (advimman/lama, Apache-2.0); trained on Places365, whose images are licensed for non-commercial research. Erik's decision (D2, Oct 13): accepted for M1; revisit before any commercial release.

## Not done

The agent started an extra probe of non-512 input sizes; it was cancelled. The graph declares 512×512 inputs, so treat the size as fixed.
