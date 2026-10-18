# Where the golden fixtures come from

Two real Wikimedia Commons photos, both CC0. The first has one obvious removable object on a simple
background; PC-06 Part A replaced the synthetic fixture MG-04 had to fall back on (no network in that
run). The second is a person against a busy background, which MG-10 added for the Select Subject
golden test.

| File | What it is |
| --- | --- |
| `input.png` | 448x448, 8-bit RGB, 288,217 bytes. The photo, cropped and downscaled. |
| `mask.png` | 448x448, 8-bit grayscale, 10,217 bytes. Hand-made over the object: 255 where the object is, 0 elsewhere, soft edge. |
| `expected.png` | 448x448, 8-bit RGB, 279,192 bytes. The answer the first good LaMa run produced, kept after looking at it. |
| `input-person.png` | 768x512, 8-bit RGB, 514,881 bytes. The second photo, downscaled. |
| `mask-buoy.png` | 448x448, 8-bit grayscale, 4,116 bytes. The subject of `input.png`, from the real BiRefNet. |
| `mask-person.png` | 768x512, 8-bit grayscale, 9,558 bytes. The subject of `input-person.png`, same model. |

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

## The second photo (MG-10)

- File page: <https://commons.wikimedia.org/wiki/File:Sitting_And_Smiling_(Unsplash).jpg>
- Original file: <https://upload.wikimedia.org/wikipedia/commons/2/23/Sitting_And_Smiling_%28Unsplash%29.jpg>
- Title: "Sitting And Smiling (Unsplash).jpg" - a person sitting outdoors, hair blown across the face
  by the wind, against a busy background of sky and vegetation.
- Author: [Brooke Cagle](https://unsplash.com/@brookecagle), own work, released to Unsplash.
- Licence: **CC0 1.0 Universal** (Public Domain Dedication). Read from the Commons API `extmetadata`
  on 2026-10-17: `LicenseShortName=CC0`, `UsageTerms=Creative Commons Zero, Public Domain Dedication`,
  `AttributionRequired=false`.
- Dated 2017-03-19 on the file page. Original 5370x3580 JPEG.
- RS-01 downloaded it as `photo-person-cc0.jpg` (1920x1280) and used it for the Select Subject
  research; the same file is the source here, so the two tasks measure the same subject.

Why this photo: the hard part of a matte is the hair and the fine detail against a busy background,
which is exactly what amendment 18's soft edge is for. A buoy on open water would pass even if the
runner threw the edge away.

## How `input-person.png` was made

1. `photo-person-cc0.jpg` (1920x1280, RS-01's copy) opened with PIL and converted to RGB.
2. Resized to 768x512 with PIL `LANCZOS` and saved as an 8-bit RGB PNG: 514,881 bytes.

Why 768x512: the mask task's `max_pixels` is 1,048,576, and 768x512 is 393,216 - comfortably inside
it, and the same aspect ratio as the source, so nothing is stretched twice.

## How `mask-buoy.png` and `mask-person.png` were made

By the real model, through a **Python** run of the same contract - deliberately not by this crate, so
the golden test compares CAIM's runner against an independent implementation rather than against a
copy of its own output:

- `birefnet_lite.onnx`, 224,005,088 bytes, SHA-256
  `5600024376f572a557870a5eb0afb1e5961636bef4e1e22132025467d0f03333` (the catalog entry), installed
  and acknowledged with `caim models install birefnet --accept-note`.
- `onnxruntime` 1.31.0, `CPUExecutionProvider`, `enable_cpu_mem_arena = False` (decision D12), Python
  PIL 12.3.0 and numpy 2.5.4.
- Preprocessing exactly as `agents/models/birefnet.md` (RS-01) verified: PIL `BILINEAR` stretch to
  1024x1024 (aspect not kept), RGB, /255, mean (0.485, 0.456, 0.406), std (0.229, 0.224, 0.225),
  NCHW.
- Post-processing: `sigmoid(logit) * 255`, clamped and rounded at 1024x1024, then a two-tap
  half-pixel-centre bilinear resize back to the fixture size, rounded again. No threshold - the
  intermediate values are the soft edge amendment 18 asks for.
- Written as 8-bit grayscale PNG.

The generator script was throwaway (`.codex-tmp/make_masks.py`, outside the repo). The check that the
two implementations agree is the golden test itself: `IoU >= 0.95` against these files, and a mean
distance of at most 2.0 levels a pixel.

Foreground fraction (pixels at 128 or above) is 0.1026 for the buoy and 0.2572 for the person, against
RS-01's 0.1027 and 0.2573 for the same two photos at their own sizes - the same subject, found twice.
Soft-edge pixels (between 1 and 254) are 1,885 on the buoy and 6,919 on the person.

The model files themselves are never committed, and neither are the source JPEGs.

## Licence

Both photos are CC0 1.0 Universal, so every fixture derived from them carries no attribution
obligation; they are offered to the project under CC0 - use, copy and re-license them freely.

The models that produced `expected.png`, `mask-buoy.png` and `mask-person.png` are *not* CC0. Their
weight licences and their training-data notes are in `docs/models/lama.md`,
`ATTRIBUTION.md` and the catalog entry, and no model file is ever committed here.

## What the golden test checks

`cargo test --test golden -- --ignored --nocapture` prints:

> 141018 pixels outside the mask are identical; inside, 0.000 levels a channel from the committed
> answer, and the masked area moved from 65.6 to 23.1 away from the background colour

and, for the two mask runs:

> buoy: session load 3724 ms, first run 9797 ms (13775 ms wall), warm run 8482 ms; IoU 0.9997, mean
> 0.012 levels from the committed matte, 1885 soft-edge pixels, warm-run drift 0 levels
>
> person: session load 3785 ms, first run 9868 ms (13928 ms wall), warm run 8608 ms; IoU 0.9996, mean
> 0.023 levels from the committed matte, 6919 soft-edge pixels, warm-run drift 0 levels

Those timings are from a debug build on the CraftML PC (Ryzen 9 7950X); a release build runs the same
model in about half the time, and RS-01 measured 3.6-3.8 s to load and 4.06-4.29 s warm with the
arena off.
