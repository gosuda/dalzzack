### Output

The same command prints differently for a person and for a program:

- In a terminal: indented JSON, every field, no size limit.
- In a pipe: compact JSON, a short form, and a size limit. This is what an agent reads.

The short form leaves out what most runs do not need, and it stays one JSON document. `navigate` drops `document` for a normal load, `describe` and `console-logs` print rows as strings, `exec-js` prints only `status` and `result` unless `--frame`, `--world`, or `--target` was given, `list` prints the fields an agent reads, and `null` fields and long `data:` URLs go. A failure is never shortened.

- `--full` — the complete result, with no size limit. `DALZZACK_OUTPUT=full` does the same for every command of a script. On `get-frames`, also the complete `name` attributes.
- `--pretty` — indented JSON in a pipe.
- `--max-output N` — above N bytes (default 16384 in a pipe), write the output to a file and print `status`, `truncated`, `bytes`, `saved_to`, `keys`, and a `preview`. 0 turns this off. A `--max-length` you set is respected. Saved files are in the data folder and are deleted after a day.

Files go to `%LOCALAPPDATA%\dalzzack` on Windows and `~/Library/Application Support/dalzzack` on macOS: `sites/<id>/` for screenshots, `logs/` for daemon logs (7 days), `output/` for saved output. `DALZZACK_LOG=debug` gives a longer log.

### Output filtering (every command that prints one JSON document)
- `--field PATH` (repeatable) — print one value: `--field status`, `--field document.hops[0].status`, `--field "instances[].id"` (`[]` fans out). Several → an object keyed by path. Missing → `null`. **A `status: "failed"` result is printed whole with exit code 1** — a filter never turns an error into a plausible `null`.
- `--jq EXPR` — real jq, built in (no `jq` binary needed): `--jq '.navigations[] | {status, final_url}'`. One output per line; sees failures too.
- `--raw` — strings without quotes (like `jq -r`); a fanned-out string list prints one per line.
