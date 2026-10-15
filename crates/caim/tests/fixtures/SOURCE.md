# Where the golden fixture comes from

**These three files are synthetic. They are not a Wikimedia Commons photo.**

MG-04 asks for one public-domain or CC0 photo from Wikimedia Commons with an obvious object on a
simple background. The agent that wrote this ran in a sandbox with no network access at all, so
nothing could be fetched. Rather than leave the evaluation gate untested, the fixture below was
generated from the recipe in this file. Everything it proves holds; what it does not prove is how
LaMa behaves on a real photograph. See "Swapping in a real photo" at the bottom.

| File | What it is |
| --- | --- |
| `input.png` | 256x256, 8-bit RGB, 114 KB. The "photo". |
| `mask.png` | 256x256, 8-bit grayscale, 336 B. Hand-made: 255 where the object is, 0 elsewhere. |
| `expected.png` | 256x256, 8-bit RGB, 38 KB. The answer the first good LaMa run produced. |

## How `input.png` and `mask.png` were made

A plain script, no model and no third-party image was involved:

- Background: a green field, red `118 + 14*y/256`, green `152 + 10*x/256 + 6*y/256`, blue
  `96 - 10*y/256`, plus deterministic noise of -5..5 from a linear congruential generator seeded
  with `20261014`.
- The object: a dark blue disc (RGB 44, 62, 168) of radius 38 centred at (128, 120), ringed to
  radius 41 by near-black (18, 20, 28).
- The mask: 255 within radius 45 of (128, 120), 0 everywhere else. It is drawn by hand, so it is
  slightly larger than the object, the way a person selecting with a lasso would leave it.

## How `expected.png` was made

The real model, on this machine, on 2026-10-14:

- `lama_fp32.onnx`, 208,044,816 bytes, SHA-256
  `1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6` (the catalog entry).
- ONNX Runtime 1.31 through `ort` 2.0.0-rc.14, CPU execution provider, one run of
  `lama::Pipeline::run_fill` with no cancellation.
- The first job took 8,873 ms in a debug build, model load included.

## Licence

Nothing here is third-party work: the images come from the recipe above, so they carry no licence
obligation. They are offered to the project under CC0 - use, copy and re-license them freely.

The model that produced `expected.png` is *not* CC0. Its own weights licence is in
`docs/models/lama.md`, and the model file itself is never committed.

## Swapping in a real photo (Erik, before this fixture is trusted)

1. Pick a public-domain or CC0 photo on Wikimedia Commons with one obvious object on a simple
   background - a bird on a wire, a sign on grass.
2. Downscale to at most 768 px on a side and save as `input.png` (8-bit RGB). Keep it under 300 KB.
3. Draw `mask.png` by hand at the same size, 8-bit grayscale, 255 over the object.
4. Delete `expected.png` and make a new baseline: a throwaway `#[ignore]`d integration test
   that calls `lama::Pipeline::run_fill` on the pair and writes `image::encode(&filled.frame)` to
   `expected.png`. Look at the result before keeping it - the object has to be plainly gone.
5. Replace this section with the photo's URL, author, licence and the date it was fetched.
