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

## How CAIM runs it (MG-04)

The runner is `crates/caim/src/lama.rs`. The maths is in plain functions so it can be tested without the 200 MB graph;
`ort` 2.0.0-rc.14 with the CPU execution provider runs the graph itself.

1. **Mask box.** The bounding box of every pixel with `mask > 0`. An empty mask returns the input unchanged and the
   model never loads or runs.
2. **Crop.** The box plus padding of `max(32 px, 25% of the box's longer side)`, grown towards a square, then
   clamped to the image one side at a time and centred on the box. A square crop keeps the 512x512 resize from
   squashing the subject; where the image is too short (or too narrow) for the square, that side is the image's whole
   height (or width), so a 400x20 box in a 2000x300 image gets a 600x300 crop rather than 600x160.
3. **Resize to 512x512.** Area averaging for a shrink, bilinear for a growth, one plane at a time.
4. **Normalise.** 8-bit input divides by 255, 16-bit by 65535, so `image` is 0-1 as the tensor table above asks.
   Only 3- and 4-channel images are in the contract; anything else is refused with a 400 before the model is loaded.
   Alpha never reaches the graph - it is handled when the fill is put back.
5. **Model mask.** `mask > 0` becomes 1.0, then dilated by 4 px at 512 scale, so LaMa repaints the edge of the
   selection as well as the hole inside it. When the crop is bigger than 512 on a side, a model pixel is covered if
   **any** source pixel in its area is (a max-pool), so a 1-2 px stroke such as a wire is still a hole at 512; when it
   is smaller, each model pixel samples the source pixel under its centre. A smoothed mask would lose a thin selection
   before the dilation gets it back.
6. **Run.** One call at 512x512. A `DELETE` during the run reaches ONNX Runtime's run termination, so the inference
   stops in flight.
7. **Back.** The output is clipped to 0-255 and divided by 255, then resized back to the crop's size.
8. **Put the fill in at full strength, by the original mask only** - never the dilated mask the model was shown.
   Where `mask > 0` the pixel is the fill, whatever the coverage; where `mask == 0` the pixel is skipped, so it comes
   back bit for bit, which is what SPEC amendment 11 promises. **CAIM does not blend by coverage: the client does**
   (SPEC amendment 16). PhotoCraft composites the "Generative Fill" layer with the mask's coverage as its alpha, so a
   feathered edge fades exactly once; if CAIM blended too, a 50% pixel would get 25% of the fill. In a 16-bit image the
   untouched pixels keep their 16-bit precision; the fill itself is 8-bit precision scaled up, which MG-04 accepts.
   Alpha inside the mask is set fully opaque; outside it is untouched.

A hole much wider than 512 px is downscaled into the window and comes back softer than the rest of the image. Accepted
for M1; tiling is CAIM-13.

The session is loaded on the first job that needs one and kept, so the load is paid once per process. `/result`
reports `model: "lama"`, `device: "cpu"`, `provider: "local"` and the real elapsed time.

`crates/caim/tests/golden.rs` is the evaluation gate amendment 14 asks for: same size, depth and channels; every
pixel outside the mask identical to the input; inside the mask, a mean absolute difference of at most 2/255 against
the committed `expected.png`; and inside the mask, closer to the colour of the background around it than the object
was. It needs the model, so it is `#[ignore]`d - run it with `cargo test -- --ignored`.
