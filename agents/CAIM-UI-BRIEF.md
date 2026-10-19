# CAIM window: UI spec for MG-13 (1d, updated Oct 19, 2026)

Owner: Erik. Design: Claude. Build: MG-13 (window), with MG-12 (resume, `app.json`, activity log, merged) and PC-10 (PhotoCraft's "Open Craft AI Manager" button). SPEC amendment 18, item 4.
Canvas: Claude Design "CAIM window 1d" (three artboards, private to Erik). **This file is the source of truth for builders:** every string, state and rule is here.
Matched to `craft-ai-manager` `main` at `8572a76` (MG-11b merged): two catalog models, three features, CUDA `supported` for both, CUDA libraries an optional 1.8 GB download.

## 1. What the window is for

People open CAIM rarely, from PhotoCraft's button, to answer three questions: **is it working? / what can it do, how do I add more? / something broke, what do I do?** Everything else (address, files, log) is diagnostics behind **Details**. Erik's taste: Paul Rand, Dieter Rams, Bauhaus, Nordic. Don't conflate pretty with functional.

Rejected directions (Oct 15): 1a sidebar + cards (a 2010s control panel, no hierarchy), 1b grouped rows (reads as Settings), 1c connection map (pretty, noisy, depicts plumbing).

## 2. Visual system

| Token | Value | Use |
|---|---|---|
| ground | `#F4F2ED` | window background (light only for M2) |
| ink | `#1A1A18` | text, rules under progress, outlined buttons |
| grey | `#6B6A65` | secondary text (5:1 on ground) |
| rule | `#D9D5CC` | 1 px hairlines between sections and rows |
| accent | `#B4400C` | **only** where the user must act: primary button, licence note bar, "needs you" dot. Never for progress. |

- **Type:** three sizes only: 28/36 (status sentence, weight 500), 15/24 (body; names weight 500), 12/16 (meta, footer). Monospace only for paths and the address.
- **Font:** Segoe UI loaded **at runtime** from `C:\Windows\Fonts\segoeui.ttf` (and `seguisb.ttf` for 500/600), falling back to egui's default font if missing. No font file is committed or bundled (Inter is OFL-1.1, outside the auto-allowed licences; needs Erik's OK if wanted).
- **Spacing:** 8 px grid. Content column max 560 px, centred, 40 px side padding, 48 px top. 40 px between the status block and the feature list; 16 px row padding.
- **No cards, panels, shadows, icons or badges.** Hierarchy comes from type size, weight, grey and space. Licence status is text in the meta line, not a badge.
- **Buttons:** primary = accent fill, white text, 48 px tall, 4 px radius. Secondary ("Add · 198 MB") = 1 px ink outline, 40 px. Tertiary ("Not now", "Cancel", "Details", "Remove") = underlined text. Hit targets at least 40 px.
- **Progress:** 2 px ink line along the bottom of the row, plus a percentage at the right in tabular figures.
- **Window:** native title bar "Craft AI Manager", default size 640×800, min 480×560; resizes fluidly (the column stays ≤ 560 px).

## 3. Layout (top to bottom)

1. **Status sentence** (28 px): bold part + grey part. Below it, one 12 px line: activity, or what's happening.
2. **Action block** (only in "needs you" states): optional licence note, one primary button, one 12 px explanation.
3. **Feature list**: one row per *catalog model*, listing the features it enables, separated by hairlines.
4. **Graphics card offer** (only when an NVIDIA card is present and the CUDA libraries are not installed).
5. **Footer** (12 px grey): `This computer only · <device> · Closing this window turns AI features off · Details`. Details expands a definition list above the footer line.

### Feature rows (from `catalog/models.toml`)

| Catalog id | Row title (PhotoCraft's names) | Purpose line | Meta line |
|---|---|---|---|
| `lama` | Generative Fill | Removes what you paint over and fills the gap. | LaMa v1 · 198 MB · non-commercial use only |
| `birefnet-lite` | Select Subject · Remove Background | Finds the main subject, to select it or mask out the rest. | BiRefNet lite v1 · 214 MB · non-commercial use only |

- Feature titles come from a small map in the window code keyed by catalog `task` (`fill_masked_area` → "Generative Fill"; `subject_mask` → "Select Subject · Remove Background"). Model name and version come from the catalog.
- "non-commercial use only" appears when the entry has an `[model.override]`; models without an override show nothing there.
- **Sizes use Windows units** (1024-based, labelled MB/GB) so they match Explorer: 208,044,816 B = 198 MB; 224,005,088 B = 214 MB; CUDA total 1,915,544,656 B = 1.8 GB. One decimal under 10 GB, none above 10 MB.
- Feature count in the status sentence counts features, not models: LaMa = 1, BiRefNet = 2.
- Right side of the row, by state: `Add · <size>` button (not installed) · `47%` (downloading) · `Ready` (installed, verified, acknowledged) · `Working` + progress line (job running) · `● Needs repair` / `● Paused · 61%` / `● Needs your OK` in ink with an accent dot (needs you).
- There is **no "Add features…" link**: every catalog entry is always listed, so uninstalled ones carry their own Add button. Hide nothing.

## 4. States and exact copy

### 4.1 First run / adding a feature (artboard 1)

| Situation | Bold | Grey | 12 px line |
|---|---|---|---|
| nothing installed | Almost ready. | Add the AI features you want. Each downloads once, then works offline. | Nothing is installed yet. |
| downloading anything | (unchanged) | (unchanged) | Downloading. You can keep working in PhotoCraft. |
| ≥1 installed, no job yet | Ready. | PhotoCraft can use N AI feature(s). | Nothing has run yet. |

**Adding a model with an override** (both today):
1. `Add · 198 MB` opens the note **inline under that row**: 2 px accent bar on the left, heading "Non-commercial use only", the catalog's `note` text **verbatim**, a checkbox "I understand it's for non-commercial use only.", primary button `Download · 198 MB` (disabled and grey until ticked), and "Not now".
2. Download: progress line + "Downloading 93 of 198 MB · checked as it arrives" + "Cancel".
3. Done → row shows `Ready`, status sentence updates. The acknowledgement is written with the download (`store::write_acknowledgement`).

A model **without** an override skips step 1: `Add` starts the download directly.

### 4.2 Ready and working (artboard 2)

| Situation | Bold | Grey | 12 px line |
|---|---|---|---|
| idle | Ready. | PhotoCraft can use 3 AI features. | Last: Select Subject, 4 min ago · 0.3 s on the graphics card |
| job running | Working. | Selecting the subject for PhotoCraft. / Filling the masked area for PhotoCraft. / Removing the background for PhotoCraft. | Started 2 s ago · usually about 5 s on the processor |

- The activity line reads the newest line of `activity.jsonl` (task, device, `fallback_reason`, `elapsed_ms`, time). Time is relative ("just now", "4 min ago", "yesterday"). Seconds with one decimal under 10 s.
- Device words: `cpu` → "on the processor"; `cuda` → "on the graphics card". When `fallback_reason` is set, append ", because …":

| `fallback_reason` | Words after "because" |
|---|---|
| `backend_unavailable` | the graphics card libraries aren't installed |
| `backend_init_failed` | the graphics card wouldn't start |
| `insufficient_memory` | the graphics card didn't have enough free memory |
| `unsupported_model` | this model isn't cleared for the graphics card yet |
| `execution_failed` | the graphics card failed partway, so it ran again on the processor |

- "usually about N s" comes from the median `elapsed_ms` of the last 5 finished jobs of that task on that device; omit it with fewer than 2.
- A fallback is **not** an error: no accent, no action.

### 4.3 Graphics card offer (artboard 2, tweak "gpu")

Shown only when `cuda::status` finds an NVIDIA card and the libraries are not installed. Hidden on machines without one (Details says "No NVIDIA card found · features run on the processor").

- Title "Use your graphics card"; text "<device name> found. AI features run many times faster on it." (measured on the 4090, MG-11b: Select Subject 4.5 s → 0.27 s warm, Generative Fill 1.36 s → 0.12 s); meta "NVIDIA CUDA and cuDNN libraries · 1.8 GB · NVIDIA's licences"; button `Add · 1.8 GB`.
- `Add` opens the terms inline (accent bar): "NVIDIA allows these libraries to be passed on, but they aren't open source. Without them every feature still works, on the processor." + link "Read NVIDIA's terms" (shows the two notes from `cuda.rs`: CUDA Toolkit, cuDNN SLA) + checkbox "I accept NVIDIA's licence terms." + `Download · 1.8 GB` + "Not now".
- Downloading: progress line + "Downloading 0.7 of 1.8 GB · features keep running on the processor meanwhile" + "Cancel". Calls `cuda::install(root, accept_note = true, …)`.
- Done: the section disappears; the footer device becomes the card name.

### 4.4 Needs you (artboard 3)

One problem at a time, the most serious first: damaged > note changed > paused download > disk full. The status sentence becomes the problem; one primary button fixes it; the affected row shows the accent dot. Other features keep working and the 12 px line says so when true.

| Problem (how detected) | Bold | Grey | Button | 12 px explanation | Row |
|---|---|---|---|---|---|
| hash mismatch (`verify_model` fails, or a job reports it) | Select Subject and Remove Background can't run. | Their model file is damaged. | Repair · 214 MB | Downloads BiRefNet lite again and checks it before use. | ● Needs repair |
| `.part` + `.part.json` exist, no download running | Download paused. | Generative Fill is 120 of 198 MB in. | Resume | Checks the part already here, then fetches only the rest. | ● Paused · 61% |
| installed, but `is_acknowledged` is false (note text changed) | Generative Fill is paused. | Its licence note changed. Read it to keep using it. | Continue with Generative Fill (disabled until the note's checkbox is ticked) | Nothing is downloaded again. | ● Needs your OK |
| write fails with no space | Download stopped. | There isn't room for the rest of Generative Fill. | Try again | Free up space on C: first. The part already downloaded is kept. | ● Paused · 61% |

- Repair = remove the bad file, then the normal install path (with resume). Errors show only plain sentences; the raw error goes under Details › What happened.
- Don't offer "Choose another folder": there is no folder setting in the UI.

### 4.5 Details (every artboard)

| Label | Content |
|---|---|
| Models | one line per installed model: "LaMa v1 · 198 MB · checked Oct 13" + `Check again` (`verify_model`) + `Remove` (`remove_model`) |
| Models folder | path from `store::models_dir()` + total size (`folder_usage`) + `Open folder` |
| Graphics card | "<name> · CUDA ready · 21.6 of 24 GB free" / "<name> · libraries not added, so features run on the processor" / "No NVIDIA card found · features run on the processor" |
| Address | `127.0.0.1:<port>` · this computer only · up 3 h 12 min |
| Connection file | path of `endpoint.json` |
| Activity | "Last 1,000 jobs are kept" + `Open activity log` |
| What happened | (needs-you states only) the raw error, monospace |
| Version | 0.1.0 |

On a needs-you state, Details also offers `Copy details for a bug report` (version, OS, device, the raw error, last 20 activity lines; never the token or any path under the user's profile beyond `%LOCALAPPDATA%\CraftAI`).

Example values on the canvas (412 MB, 21.6 GB free, port 41234, uptimes, "4 min ago") are placeholders; the window shows live values.

## 5. Behaviour rules for the builder

1. **The window is the server.** `caim open` starts serving and shows this window in the same process. Window actions call the library functions directly (`models::install_model`, `verify_model`, `remove_model`, `cuda::install`), not the CLI.
2. **Remove while serving:** `remove_model` refuses while a live manager answers on the endpoint port. From the window, the process *is* that manager: stop routing jobs to the model, wait for its running job (or cancel it), drop its session, then delete. A job arriving for a removed model gets the normal 400 "…isn't installed." sentence.
3. **Never block the UI thread.** Downloads, hashing, CUDA install and verify run on worker threads with progress callbacks; the window repaints at most every 100 ms. Closing during a download keeps the `.part` files (MG-12 resume).
4. **Closing the window stops CAIM** (no tray until CAIM-09). The footer says so. If a job is running, closing asks once: "A job is running for PhotoCraft. Close anyway?" with "Close" / "Keep open".
5. **Licence notes are shown verbatim from the catalog**, and acknowledgement is tied to the note's hash (already true in `store.rs`), so editing a note forces a fresh OK. That is intended (see 4.4).
6. **Errors never panic** and never show Rust error text in the main column.
7. **Accessibility:** every control reachable by keyboard (Tab order follows the column), visible focus ring (2 px ink), accesskit labels for icon-free buttons, text contrast ≥ 4.5:1 (the tokens above pass).
8. **Tests:** a state-model unit test per row in sections 4.1 to 4.4 (inputs: catalog + store + activity + cuda status → expected strings), plus a test that sizes format as in section 3. No pixel snapshots needed for M2.

## 6. Open for Erik

1. **Closing = stop** (rule 4)? The alternative is to keep serving with no window, which needs the tray (CAIM-09) to be findable again.
2. **LaMa's catalog note** still says "Use it for personal work only." Proposed text (shown in 4.1 and 4.4): "LaMa's code and weights are Apache-2.0, but it was trained on Places2, whose images are licensed for non-commercial research and education. Not for paid or client jobs." Changing it makes existing installs ask for one fresh OK (4.4, row 3). Also: the `reason` field and the D2 line in SPEC amendment 13 still say "personal use".
3. **Font:** Segoe UI from the system (default above), or bundle Inter (OFL-1.1, needs your OK)?

## 7. Future: permission for third-party models (Oct 15; not M2)

Like Ubuntu's "install third-party software" choice: one opt-in instead of a prompt per model.
- **One switch, off by default:** "Allow models with extra licence terms." Asked once on first run, where the per-model acknowledgment sits now; afterwards it lives under Details.
- **Clean** models (permissive code, weights and training data) install with no prompt. **Flagged** models (an Erik override, e.g. LaMa, BiRefNet) need the switch on, and each still shows its note once before its first download (CAIM-11). **Web providers** (step 4) get a separate consent with its own switch and the "uploads to X" notice, because the image leaves the computer.
- **Switch off:** flagged models stay listed, greyed, with "Needs permission for models with extra licence terms · Change". Don't hide them.
- **One source of truth:** the same manifest override flag the CI licence gate checks (amendment 14).
- Erik's aim: schools, libraries and similar non-commercial users get unrestricted access. A fill model with clean training data (D7) removes the prompt for them entirely.

## Related
SPEC amendments 14, 18 (M2), 19 (MAS keeps macOS paths in mind: `~/Library/Application Support/CraftAI/`); FEATURES CAIM-02, -03, -05, -09, -10, -11; tasks MG-12, MG-13, PC-10.
