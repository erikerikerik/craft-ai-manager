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


**Status: pre-alpha, fake backend only.** MG-01 built the server: `caim serve` binds `127.0.0.1` on a free port, publishes
the endpoint file, and speaks the whole job protocol of SPEC amendment 11. No models are installed yet, so the only
backend is a built-in fake one that fills a masked area with the mean colour of the pixels around it. The model store
is MG-02 and the ONNX runner is MG-04.

## Running it

```text
caim                        # the same as: caim serve
caim serve --fake-backend   # jobs run on the built-in mean-colour fill
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
your own account. If `CRAFT_AI_ENDPOINT` points somewhere else, the file inherits that folder's permissions instead,
so point it at a folder only you can read.

The server answers only `127.0.0.1`, only with the right `Authorization: Bearer <token>` header, and only when
`Host:` is exactly `127.0.0.1:<port>` - which is what stops a page in your browser, or a DNS rebinding, from reaching
it. One request per connection, at most 16 connections at once, a 30 s read timeout, `Connection: close`, and a
`Content-Length` on every reply. A request body is at most 1 GiB, an image at most 64 megapixels, and eight jobs run
at a time; a finished result is kept for ten minutes, or until it has been read, whichever comes first.

Without `--fake-backend` the manager lists no tasks and answers a job with "No model is installed for
fill_masked_area yet", which is how PhotoCraft knows to grey the menu out.

### Stopping it

Press Enter in the terminal: that stops the server cleanly and removes `endpoint.json`. Ctrl+C stops the process too,
but MG-01 adds no dependency and `std` has no signal handler, so on Windows Ctrl+C ends the process where it stands
and leaves the file behind. That is safe - a client that reads a stale file gets a refused connection and re-reads
it, and the next start replaces the file - but Enter is the clean stop.
