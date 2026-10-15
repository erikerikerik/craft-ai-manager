# Craft AI Manager (CAIM)

Craft AI Manager is a small local service that runs AI models for [PhotoCraft](https://github.com/erikerikerik/photocraft)
over a loopback HTTP API. It owns the model catalog, downloads (SHA-256 checked), update checks, disk usage,
device choice and API keys, so PhotoCraft itself stays free of models, GPU runtimes and keys. It listens only
on `127.0.0.1`, requires a token that is generated fresh on each start, and sends no telemetry. The same
protocol can later serve other Craft apps (LightCraft, FilmCraft).

CAIM is part of the PhotoCraft AI project. The contract it implements lives in the PhotoCraft fork:
`agents/SPEC.md` in [erikerikerik/photocraft](https://github.com/erikerikerik/photocraft), especially the v1.1
amendments 11 (job protocol v1 wire details), 13 (model licences, including training data) and 14 (CAIM runtime
patterns). The work list is that repo's `agents/TASKS.md`, the `MG-*` rows.

**Licence:** PolyForm Noncommercial 1.0.0 - free for personal, education, research and nonprofit use;
commercial use needs a licence from Erik Kristianson. See `LICENSE.md`.


**Status: pre-alpha, one real model.** MG-01 built the server: `caim serve` binds `127.0.0.1` on a free port,
publishes the endpoint file, and speaks the whole job protocol of SPEC amendment 11. MG-02 built the model store: a
catalog compiled into the binary (`crates/caim/catalog/models.toml`), the licence gate that decides what CAIM may offer,
SHA-256-checked downloads, and the `caim models ...` commands. MG-04 runs the first model for real: `fill_masked_area`
puts `lama_fp32.onnx` through ONNX Runtime on the CPU (`crates/caim/src/lama.rs`). The built-in fake backend - a mean
colour fill - is still there behind `--fake-backend`, and it behaves exactly as MG-01 shipped it.

## Running it

```text
caim                        # the same as: caim serve
caim serve --fake-backend   # jobs run on the built-in mean-colour fill
caim models list            # what the catalog offers, and what is installed
caim models install <id>    # download one model, hashing it while it arrives
caim models verify <id>     # re-hash an installed model against the catalog
caim --version
```

`serve` binds `127.0.0.1:0` (a port the operating system picks), makes a token of 32 random bytes, and writes
`endpoint.json` atomically - a temp file in the same folder, then a rename over the old one - with:

```json
{ "version": 1, "port": 41234, "token": "<64 hex characters>" }
```

Where that file lives:

| Platform | Path |
|---|---|
| Windows | `%LOCALAPPDATA%\CraftAI\endpoint.json` |
| macOS | `~/Library/Application Support/CraftAI/endpoint.json` |
| Linux | `$XDG_DATA_HOME/CraftAI/endpoint.json`, else `~/.local/share/CraftAI/endpoint.json` |

`CRAFT_AI_ENDPOINT`, set to a file path, moves it; that is how the tests keep several servers apart. On Windows the
file inherits the per-user permissions `%LOCALAPPDATA%` already carries, so the token inside it is readable only by
your own account; on Linux and macOS the file is created with mode `0600`, so it is readable only by your own account
there too, even in the moment between the temp file and the rename. If `CRAFT_AI_ENDPOINT` points somewhere else, the file
inherits that folder's permissions instead, so point it at a folder only you can read.

The server answers only `127.0.0.1`, only with the right `Authorization: Bearer <token>` header, and only when
`Host:` is exactly `127.0.0.1:<port>` - which is what stops a page in your browser, or a DNS rebinding, from reaching
it. One request per connection, at most 16 connections at once, a 30 s read timeout, `Connection: close`, and a
`Content-Length` on every reply. A request body is at most 1 GiB, an image at most 64 megapixels, and eight jobs run
at a time; a finished result is kept for ten minutes, or until it has been read, whichever comes first.

With no backend at all - no `--fake-backend`, and no installed and acknowledged LaMa - the manager lists no tasks and
answers a job with "No model is installed for fill_masked_area yet", which is how PhotoCraft knows to grey the menu out.


### Running it with a real model

```text
caim models install lama --accept-note   # once: 200 MB, hashed while it arrives
caim serve                               # jobs now run on LaMa, on the CPU
```

`caim serve` without `--fake-backend` offers `fill_masked_area` in `/v1/tasks` only when LaMa is **installed** - the
file is there, its size is what the catalog says, and a `verified.json` names the catalog's SHA-256 - **and**
acknowledged, because LaMa carries the Places365 note. Starting the server never re-hashes the 200 MB file; it reads
the record. Anything else keeps the no-backend answer above.

The ONNX session is loaded lazily, on the first job that needs one, and kept for every job after it. The first job pays the
model load - 7.5 s of a 9.0 s debug-build job, against 1.2 s for the next one - so only the first job is slow.

Progress comes in stages, and `message` names the stage the manager is in:

| stage | `progress` | what is happening |
|---|---|---|
| prepare | 0 to 0.2 | crop, pad, resize to 512x512 |
| load model | 0.25 | the first job only |
| run | 0.3 to 0.9 | the graph: about a second on a desktop CPU |
| finish | 0.9 to 1.0 | resize back and blend |

A `DELETE /v1/jobs/<id>` stops a job wherever it is. A queued job never starts, and the model is not even loaded for
it. A job inside the run is cancelled through ONNX Runtime's own run termination, so the inference stops in flight
instead of running to its end; anywhere else the cancel lands at the next stage boundary. A cancelled job has no
result: its state is `cancelled`, and the result endpoints say so.

**The 512 px limit.** LaMa takes one fixed 512x512 window, so the runner crops the mask's bounding box plus padding,
resizes that to 512x512, runs, and resizes the answer back. A hole much wider than 512 px is downscaled to fit and
comes back softer than the rest of the image. That is accepted for M1; tiling windows across a large hole is CAIM-13.

The result of a job reports the real provenance: `model` `lama`, `device` `cpu`, `provider` `local`, and the
milliseconds the run took.

### Stopping it

Press Enter in the terminal: that stops the server cleanly and removes `endpoint.json`. Ctrl+C stops the process too,
but MG-01 adds no dependency and `std` has no signal handler, so on Windows Ctrl+C ends the process where it stands
and leaves the file behind. That is safe - a client that reads a stale file gets a refused connection and re-reads
it, and the next start replaces the file - but Enter is the clean stop.

## Models

CAIM owns the model files; PhotoCraft never holds a copy. What exists is a catalog compiled into the binary,
`crates/caim/catalog/models.toml`, one `[[model]]` block per model version with the SPEC amendment 14 manifest fields:
`id`, `version`, `task`, `file`, `url` (HTTPS, pinned to a revision), `revision`, `sha256`, `size`, the three licences
(`licence_code`, `licence_weights`, `licence_training_data` - amendment 13 counts the training data), `provenance`, and
an optional `[model.override]` table (`by`, `date`, `reason`, `note`). The one entry today is LaMa; its facts and licence
note are in `docs/models/lama.md`.

```text
caim models list                        # task, size, licences, and whether it is installed
caim models install lama                # download it, hashing while it arrives
caim models install lama --accept-note  # accept a flagged model's note first, then download
caim models verify lama                 # re-hash the file on disk (startup does not)
```

Models live in the same data directory as `endpoint.json`:

| Platform | Path |
|---|---|
| Windows | `%LOCALAPPDATA%\CraftAI\models\<id>\<version>\<file>` |
| macOS | `~/Library/Application Support/CraftAI/models/<id>/<version>/<file>` |
| Linux | `$XDG_DATA_HOME/CraftAI/models/...`, else `~/.local/share/CraftAI/models/<id>/<version>/<file>` |

`CRAFT_AI_MODELS`, set to a folder, moves the whole models tree; that is how the tests keep out of the real store.

A model counts as **installed** only when three things agree: the file is there, its size is what the catalog says, and
a `verified.json` - written only after a successful SHA-256 check - names the same hash. Startup never re-hashes a
200 MB file; `caim models verify <id>` does, and a failed check removes the record so the model stops counting as
installed until it is downloaded again.

A download goes to `<file>.part` in the same folder and is hashed as it streams. A server whose `Content-Length`, or
whose bytes read, go past the catalog's `size` is refused; a hash or size mismatch deletes the `.part` file and names
the expected and the actual value. Redirects are followed - Hugging Face hands the download to a CDN - but never down
to plain HTTP. A model that is already installed makes no request at all. There is no resume yet (CAIM-03 puts resume
in S2) and cancellation waits for MG-05; while a download runs the CLI prints a percentage.

**The licence gate.** An entry is not offered - and the test over the embedded catalog fails in CI - when a field is
missing or empty, when anything says `UNCLEAR`, when the URL is not HTTPS, when `sha256` is not 64 lowercase hex, when
the file is not `.onnx` or `.safetensors`, when two entries share an id and version, or when a licence is on the deny
list: GPL, LGPL, AGPL, anything with `NC` as a licence token such as `CC-BY-NC-4.0`, OpenRAIL, `research-only`, or
`non-commercial`. A denylisted entry can be carried with a complete `[model.override]` from Erik - `by`, `date`,
`reason` and `note` - and then it needs one acknowledgment: `caim models install lama` prints the note and stops, and
`--accept-note` records `acknowledged.json` (id, version, the note's SHA-256, the time) next to the model and goes
ahead. Change the note and it needs acknowledging again. LaMa is flagged for its Places365 training data, which is
licensed for non-commercial research (decision D2 in the PhotoCraft task list).

### Exit codes

`caim --help` repeats these.

| Code | Meaning |
|---|---|
| 0 | everything is fine |
| 1 | something went wrong: the server could not start, a download failed, or a file or hash did not match the catalog |
| 2 | the arguments do not make sense |
| 3 | the model is unknown, is not installed, or its note needs acknowledging |
