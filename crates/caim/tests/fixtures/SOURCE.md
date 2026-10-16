# Where the golden fixture comes from

One real Wikimedia Commons photo, CC0, with one obvious removable object on a simple background.
PC-06 Part A replaced the synthetic fixture MG-04 had to fall back on (no network in that run).

| File | What it is |
| --- | --- |
| `input.png` | 448x448, 8-bit RGB, 288,217 bytes. The photo, cropped and downscaled. |
| `mask.png` | 448x448, 8-bit grayscale, 10,217 bytes. Hand-made over the object: 255 where the object is, 0 elsewhere, soft edge. |
| `expected.png` | 448x448, 8-bit RGB, 279,192 bytes. The answer the first good LaMa run produced, kept after looking at it. |

## The photo

- File page: <https://commons.wikimedia.org/wiki/File:Orange_buoy_in_water.jpg>
- Original file: <https://upload.wikimedia.org/wikipedia/commons/d/d7/Orange_buoy_in_water.jpg>
- Title: "Orange buoy in water.jpg" - an orange mooring buoy in the water of Hanneviken,
  Norrkila, Lysekil Municipality, Sweden, lit by the setting sun.
- Author: [W.carter](https://commons.wikimedia.org/wiki/User:W.carter), own work.
- Licence: **CC0 1.0 Universal** (Public Domain Dedication). Read from the file page's licensing
  section, and confirmed from the Commons API `extmetadata` on 2026-10-15: `LicenseShortName=CC0`,
  `UsageTerms=Creative Commons Zero, Public Domain Dedication`, `AttributionRequired=false`.
- Taken 2018-06-02, uploaded 2018-07-03. Original 4925x3283 JPEG, 9,609,272 bytes. Featured picture.
- Fetched by the local agent on 2026-10-15.

The object is the buoy: the ball, the black handle rod and loop above it, and the orange reflection
in the water below it. The background is open water, which is what makes the removal checkable.

## How `input.png` was made

1. Cropped a 3283x3283 square out of the original, box `(858, 0, 4141, 3283)`, so the buoy, its
   handle and its reflection are all inside with water around them.
2. Resized to 448x448 with PIL `LANCZOS` and saved as an 8-bit RGB PNG: 288,217 bytes.

Why 448: the task allows at most 768 px on the long side and 300 KB as PNG. The fixture is square
because the golden test grows the mask with `lama::dilate`, which only works on a square plane.
480x480 fits the pixel budget but comes to 324,706 bytes as PNG, over the limit; 448x448 is the
largest square that stays under 300 KB.

## How `mask.png` was made

Drawn by hand over the object, in 448-space, with a little margin and a soft edge. The recipe, in
PIL and numpy, reproduces the committed file bit for bit:

1. A warm-colour test on the photo: `r > 60 and r - b > 18 and r - g > 12`.
2. Keep the largest connected component: 31,433 pixels, bounding box x 182-355, y 196-429.
3. Grow it about six pixels with `ImageFilter.MaxFilter(13)`.
4. Union that with hand-placed shapes, so the thin rod and the reflection are not missed:
   an ellipse centred (266, 268) with radii (96, 84) for the ball; an ellipse centred (272, 380)
   with radii (98, 60) for the reflection; an ellipse centred (112, 66) with radii (42, 36) for the
   handle loop; a line from (112, 66) to (222, 220), 14 px wide, for the rod.
5. Soften with `ImageFilter.GaussianBlur(3)`.

The result covers 59,686 pixels, 13.3% of the frame, 39,942 of them pure 255. It touches the bottom
edge, where the reflection runs out of the crop.

## How `expected.png` was made

The real model on this machine, on 2026-10-15, from a run that was then looked at:

- `lama_fp32.onnx`, 208,044,816 bytes, SHA-256
  `1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6` (the catalog entry), installed
  and acknowledged with `caim models install lama --accept-note`.
- `ort` 2.0.0-rc.14 and `ort-sys` 2.0.0-rc.14 with the ONNX Runtime binaries they download at build
  time, CPU execution provider, no `ORT_LIB_PATH` in the environment.
- One run of `lama::Pipeline::run_fill` on `input.png` and `mask.png` with no cancellation, written
  out with `image::encode`. Debug build: 8,338 ms, model load included.
- The throwaway `#[ignore]`d test that wrote it was deleted once the fixture was settled, the way
  the previous version of this file said to.

Checked by eye before keeping it: the buoy, the handle and the reflection are plainly gone, and the
water reads as water. Machine check for leftovers: the warm-colour test from step 1 of the mask
finds 31,438 pixels in `input.png` and 0 in `expected.png`.

## Licence

The photo is CC0 1.0 Universal, so `input.png` and everything derived from it carry no attribution
obligation; they are offered to the project under CC0 - use, copy and re-license them freely.

The model that produced `expected.png` is *not* CC0. Its weights licence is in
`docs/models/lama.md`, and the model file itself is never committed.

## What the golden test checks

`cargo test --test golden -- --ignored --nocapture` prints:

> 141018 pixels outside the mask are identical; inside, 0.000 levels a channel from the committed
> answer, and the masked area moved from 65.6 to 23.1 away from the background colour
