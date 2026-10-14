# Receipt: <ID> <feature name>

- **Date:**
- **Author:** (agent name + model, e.g. "Codex, gpt-…" or "local: Codex + Strata IQ3_S")
- **Reviewer:** (must differ from the author)
- **Repo / branch / PR:**
- **Merge commit:** (full SHA)
- **CI run:** (link, or "local only")
- **Machine:** (CraftML 4090 PC / Codex cloud / CI runner), OS, `rustc --version`

## Commands run

| Command | Result | Passed / failed / ignored | Time |
|---|---|---|---|
| `cargo test -p …` | pass | | |
| `cargo clippy -p … --all-targets -- -D warnings` | pass | | |
| `cargo xtask layers` | pass | | |

## Guards

`agents/guards.txt`: N lines, all pass? (yes / no, which failed)

## Behaviour rules covered

Which B-rules this feature was tested against, and the test name for each.

## Evidence

Screenshots (path), timings, golden-image diff numbers.

## Known gaps and follow-ups
