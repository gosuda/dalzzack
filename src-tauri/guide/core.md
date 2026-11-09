## Browser Automation (dalzzack)

`dalzzack` drives a real browser window from the command line. Each command prints one JSON document.

```bash
dalzzack start --id b                                 # once; `dalzzack list` shows what runs
dalzzack navigate -i b --url "https://example.com"
dalzzack describe -i b                                # numbered screenshot in `file`, plus elements
dalzzack click -i b --text "Sign In"                  # or --selector CSS, or --x X --y Y
dalzzack fill-form -i b --selector "#email" --value "a@b.c"
dalzzack wait-for -i b --selector ".result" --state added
dalzzack get-text -i b [--selector CSS]               # read the page: the cheapest way
dalzzack screenshot -i b                              # prints a file path
dalzzack console-logs -i b --level error              # when the page misbehaves
dalzzack stop --id b
```

Run several steps in one call. It stops at the first failure and prints the last result:

```bash
dalzzack run -i b --steps - <<'EOF'
[["fill-form", "--selector", "#email", "--value", "a@b.c"],
 ["click", "--text", "Sign In"],
 ["wait-for", "--selector", ".dashboard"]]
EOF
```

Output in a pipe is short: compact JSON, the fields most runs need, and at most 16 KiB. Above that, the whole output is in the file named by `saved_to`. A failure always prints in full. `--full` prints every field. `--field PATH` and `--jq EXPR` read the full result and have no size limit, as in `--field document.hops[0].status`.

Rules:

- Read a page with `get-text`. Use `get-html` only for a value that is not on the screen.
- Run `describe` first on a new page. Open its screenshot only when the element list is not enough.
- Prefer `click --text` or `--selector` to coordinates.
- On the default engine, a Cloudflare "Verify you are human" checkbox takes neither `click` nor plain `keyboard`. Run `keyboard --key Tab --native`, then `keyboard --key Space --native`.
- Use `wait-for` instead of a sleep, and `wait-navigation` after a click that replaces the document.
- If a command times out, do not repeat it. Run `dalzzack pause -i b`, and read `dalzzack guide diagnostics`.
- `start` flags apply only to a new daemon. Use `resize` to change a live window.
- In Git Bash, set `MSYS_NO_PATHCONV=1` for a value that starts with `/`.

`dalzzack guide <topic>` prints the details. `dalzzack <command> --help` prints the flags.
Topics: instance, navigate, interact, inspect, advanced, intercept, inject, diagnostics, chrome, output, rules.
