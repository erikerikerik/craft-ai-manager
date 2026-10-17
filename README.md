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
colour fill - is still there behind `--fake-backend`; since BF-01 it follows the same full-strength rule as LaMa (below).

## Running it

```text
caim                        # the same as: caim serve
caim serve --fake-backend   # jobs run on the built-in mean-colour fill
caim open                     # start the manager, or say that one is already running
caim models list            # what the catalog offers, what is installed, and what it costs on disk
caim models install <id>    # download one model, hashing it while it arrives
caim models verify <id>     # re-hash an installed model against the catalog
caim models remove <id>     # delete one model's installed files (refused while a manager is running)
caim models where           # the models folder this run uses
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

`caim open` is the start-on-demand entry point (amendment 18, decision D5). It reads `endpoint.json` first: when a
manager is answering `GET /v1/health` on the port that file names, it prints
`Craft AI Manager is already running on 127.0.0.1:<port>. Nothing was started.` and exits 0 without binding a port.
A manager that answers `503` with its own busy sentence because all of its connection slots are taken counts as
running too: it is busy, not gone. A `503` from anything else on that port - a dev server that took the port a
crashed manager left behind - is a stranger, not a running manager, so `caim open` starts one.
With nothing answering it serves exactly what `caim serve` serves - the window comes with MG-13. PhotoCraft's
"Open Craft AI Manager" button runs `caim open`, which is why every start records where this program lives. Two
`caim open` runs launched at the same moment decide once: the first holds `open.lock` beside the endpoint file while it
starts a manager, and the second waits for that lock (up to 5 s) and then sees the manager the first one started.
A lock this machine cannot make at all - the folder is not there and cannot be created, say - is an error, not
contention: `caim open` prints what went wrong and exits 1 rather than saying another run is starting a manager
that nobody is starting.

Three more files live in that same folder. `app.json` and `activity.jsonl` are never deleted on exit; `open.lock`
exists only while a `caim open` is deciding whether to start a manager:

| File | What it is |
|---|---|
| `open.lock` | held by one `caim open` across the decision and the start, so two runs launched together cannot both serve and race to replace `endpoint.json`. Created with `create_new`, holding a random nonce; gone as soon as the server is listening. One nobody has touched for 30 s belonged to a run that died, and the next run takes it over |

| File | What it is |
|---|---|
| `app.json` | `{"version": 1, "exe": "<absolute path of this executable>"}`, rewritten atomically on every start of `serve` and `open`, so another app can find the manager and start it |
| `activity.jsonl` | one JSON line per finished job - `done`, `failed` or `cancelled` - with `time`, `task`, `model`, `version`, `device`, `device_requested`, `fallback_reason`, `elapsed_ms` and `state`; the newest 1,000 lines are kept, the file being rewritten once it passes 1,100 |

`app.json` is written just after the endpoint file. Failing to write it is a sentence on stderr, not a failed start:
the manager is still serving, it just cannot be found by path. Log fields CAIM has no value for yet - the model
version, the requested device, the fallback reason - are written as `null`, so the file keeps its shape when MG-10
and MG-11 fill them in. A line that cannot be written is a sentence on stderr and never fails the job it describes;
the log is a convenience, not a dependency.

The server answers only `127.0.0.1`, only with the right `Authorization: Bearer <token>` header, and only when
`Host:` is exactly `127.0.0.1:<port>` - which is what stops a page in your browser, or a DNS rebinding, from reaching
it. One request per connection, at most 16 connections at once, a 30 s read timeout, `Connection: close`, and a
`Content-Length` on every reply. A request body is at most 1 GiB, an image at most 64 megapixels, and eight jobs run
at a time; a finished result is kept for ten minutes, or until it has been read, whichever comes first.

Each request also has one overall deadline, however its bytes are split into reads: the whole head within 5 s, and
the body within 30 s plus 1 s for every 8 MiB of its `Content-Length` (three minutes at most). So a client that sends a
byte every few seconds cannot hold a connection slot for more than 5 s of head. A local process that keeps
reconnecting 16 sockets can still make the manager answer 503 to others; on loopback that is accepted, because such a
process could do worse to the machine anyway. When the manager answers before it has read the body - a wrong
token (401), a wrong `Host` (400), a body over the cap (413), or too many connections (503) - it closes its side for
writing and reads the rest of the body, up to its `Content-Length` and a few seconds, before closing. A client that
writes its whole request before reading still gets that answer instead of a reset connection.

**Who blends a soft selection (SPEC amendment 16).** `result.png` is the input, bit for bit, wherever the mask is 0,
and the fill **at full strength** wherever the mask is above 0, with alpha there set fully opaque. CAIM never blends by
coverage: PhotoCraft does, when it uses the mask's coverage as the alpha of the "Generative Fill" layer. If both sides
blended, a 50% feathered pixel would get 25% of the fill. The fake backend follows the same rule.

With no backend at all - no `--fake-backend`, and no installed and acknowledged LaMa - the manager lists no tasks and
answers a job with "No model is installed for fill_masked_area yet", which is how PhotoCraft knows to grey the menu out.


### Running it with a real model

**Resource limits and recovery (BF-03).** Uploads reserve a queue slot and their body bytes before the body is read;
decoded images reserve conservative working space before PNG decoding. A shared 2 GiB image-data budget covers
uploads, queued/running image work, and retained PNGs. Working space is charged at 32 bytes per pixel plus 32 MiB;
on completion that shrinks to the PNG buffer's capacity. This is accounting, not a process-RAM limit: ONNX Runtime's
session allocations, allocator overhead, and kernel socket buffers are outside it. `ServeOptions::memory_limit` can
adjust the budget. Normal encoded PNG bytes are included in that peak estimate, not charged twice; unusually large
encoded parts also get a decode-phase estimate. An idle 8000×8000 RGBA16 image fits the default budget, preserving
the advertised 64 MP ceiling. A request whose own peak exceeds the whole budget receives 413; 503 means other
requests/results are holding capacity (or the queue/record cap is full). Growing RGB axes compute their weights
on demand, avoiding a large per-output-pixel weight table. Job records and pending worker IDs are each capped at 1,024, so finished/cancelled jobs cannot grow
without bound during their ten-minute retention period.

Responses have one 30 s write deadline and are written without copying the complete PNG into another response
buffer. If the server detects a failed result write, the PNG remains available for retry until expiry; a successful
write still consumes it. This cannot detect a client disconnect after the operating system accepted every byte;
guaranteed receipt would require a protocol acknowledgement. Shutdown cancels jobs, frees queued inputs, and
interrupts socket operations through cancellation polling, including on Windows. Active CPU preparation/model
loading still stops at its next cancellation boundary. Extreme aspect ratios use the smaller intermediate when
resizing RGB data and pooling masks.

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
| finish | 0.9 to 1.0 | resize back and put the fill in |

A `DELETE /v1/jobs/<id>` stops a job wherever it is. A queued job never starts, and the model is not even loaded for
it. A job inside the run is cancelled through ONNX Runtime's own run termination, so the inference stops in flight
instead of running to its end; anywhere else the cancel lands at the next stage boundary. A cancelled job has no
result: its state is `cancelled`, its `message` is "Cancelled." with no further progress, and the result endpoints say
so. A queued job's image and mask are freed the moment it is cancelled.

**The 512 px limit.** LaMa takes one fixed 512x512 window, so the runner crops the mask's bounding box plus padding,
resizes that to 512x512, runs, and resizes the answer back. A hole much wider than 512 px is downscaled to fit and
comes back softer than the rest of the image. That is accepted for M1; tiling windows across a large hole is CAIM-13.

The result of a job reports the real provenance: `model` `lama`, `device` `cpu`, `provider` `local`, and the
milliseconds the run took.

### Stopping it

Press Enter or Ctrl+C: either one stops the server cleanly - the job that is running and any queued
one are cancelled, the listener closes, and `endpoint.json` goes away. MG-08 added a signal handler
(`ctrlc`), so Ctrl+C now does what Enter does: Ctrl+C and Ctrl+Break on Windows, a console close or
logoff there too, and SIGINT, SIGTERM and SIGHUP on Linux and macOS. A second Ctrl+C while a
shutdown is still running ends the process at once with exit code 1, and the endpoint file may then
still be there. That stays safe: a client that reads a stale file gets a refused connection, and the
next start replaces it.

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
caim models remove lama                 # delete lama's installed files (refused while a manager answers)
caim models where                       # the models folder this run uses, and what named it
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

`caim models list` ends every model line with the bytes that model's folder holds (`on disk: 208044816 bytes`) and adds
one last line with the total for the whole models folder and the folder's own path. `caim models where` prints that
folder and says what named it: `CRAFT_AI_MODELS` when that is set, the default data folder otherwise. A models folder
whose tree is deeper or broader than the walk can count is named as too many folders rather than shown as zero, and so
is one where a link sits in place of the models folder or of a model's own folder: the bytes on the other side of a
link are never counted, because they are not in the models folder.

`caim models remove <id>` deletes `<id>\<version>\`: the model file, `verified.json`, `acknowledged.json`, and any
`.part`, its `.part.json` note or `.lock` an interrupted install left behind. When that was the model's last version, the now empty `<id>`
folder goes with it. It asks no questions, because the command is already explicit. It refuses while a manager is
running: the endpoint file names a port, and a connection to that port answers, so the model may be in use. A file left
behind by a manager that died does not refuse, because the port is asked as well. What it deletes is always
`<models folder>\<id>\<version>` - the catalog gate refuses names that cannot be folder names, the removal asks the
same question again before it touches the disk, and a link in place of the models folder or of either model folder is
refused rather than followed.

A download goes to a `.part` file in the same folder as the model and is hashed as it streams. A server whose
`Content-Length`, or whose bytes read, go past the catalog's `size` is refused; a hash or size mismatch deletes the
part and names the expected and the actual value. Redirects are followed - Hugging Face hands the download to a CDN -
but never down to plain HTTP. A model that is already installed makes no request at all, and while a download runs
the CLI prints a percentage.

**Restart-safe resume (CAIM-03, MG-12).** While a download runs it keeps a note beside its bytes:
`<file>.<random>.part` and `<file>.<random>.part.json`, the note holding the url, the pinned revision, the expected
SHA-256 and the bytes written so far, rewritten atomically every 8 MiB. The next `caim models install` of the same
model re-hashes those bytes - a partial file is never trusted for its hash alone - and asks for the rest with
`Range: bytes=<n>-`. A `206` continues where it stopped, and the percentage starts at the resumed offset; a `200`
means the server ignored the range, so the download starts over from zero; any other status is an error that leaves
the bytes for the next try. A note naming another revision or another SHA-256 than the catalog asks for describes a
different file, so both files are deleted and the download starts over. The finished hash still has to match the
catalog: a mismatch deletes both files and the install fails. A download that stops short - a `Content-Length` that
runs out, or a read error halfway through - keeps what arrived and says so, and the install line names how many of
the bytes were already on disk.

The note counts the bytes it was written alongside, so a run killed between two checkpoints normally leaves more
bytes in the `.part` than the note counts. Those extra bytes are not covered by any note, so they are cut off and the
recorded prefix is what resumes. A `.part` shorter than its note is missing bytes the note vouches for, so that pair
is deleted and the download starts over. When the note counts the whole file - a run that died just after its last
checkpoint - those bytes are hashed and put in place where they are, because asking the server for
`Range: bytes=<size>-` would only be refused.

Each install writes its own `<file>.<random>.part` and its own `<file>.<random>.part.json`, created exclusively, and
renames into place only the bytes it hashed itself, so two installs at once can never put a wrong file under the
verified name. That is why the pair carries a random tag rather than the bare `<file>.part` amendment 18 names:
two installs of one model would otherwise overwrite each other's bytes and each other's note. On top of that, one
install of a model downloads at a time: it holds `<file>.lock` (created exclusively, touched every few seconds while
the download runs), and a second install is told another one is running. A lock nobody has touched for two minutes
was left by an install that died; the next install moves it aside under a name of its own (if several try at once,
one rename wins) and takes over. A `.part` nobody has written to for two minutes is cleared the same way, with one
exception: a `.part` whose note is still there and just as old is a half download that the next install can pick up,
so that pair stays until resume adopts it or the rules above reject it. An install that created the lock file itself
knows no other installer exists, so it adopts such a pair however young it is - running the same command again picks
the bytes up straight away. Only an install that took the lock over insists both halves are old, because the install
that left them might only be stalled, and a takeover never writes into the bytes it found: it copies the recorded
prefix into a `.part` of its own, because the stalled install may still hold the old file open and renaming it would
leave this install verifying a file somebody else is still appending to. A `.part` with no note, and a note with no
`.part`, are both dead and go. The file is synced to disk before it is renamed into place.

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

