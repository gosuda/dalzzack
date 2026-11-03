---
name: Bug report
about: Something broke. Help us reproduce it.
labels: bug
---

## What happened

<!-- One sentence. What did you observe? -->

## What you expected

<!-- One sentence. What should have happened instead? -->

## Reproduction

Exact CLI commands, in order:

```bash
dalzzack start --id repro
dalzzack navigate -i repro --url "..."
# ...
```

If you used a step file or batch mode, paste the JSON below.

## Environment

- `dalzzack --version`:
- OS and version:
- Install method: `npm run deploy` / `cargo install --path` / prebuilt binary / other
- Target site (if it's site-specific):

## Daemon log

Paste relevant lines from `~/.cargo/bin/logs/dalzzack_<id>_<date>.log` (path differs for non-deploy installs). Redact anything sensitive.

```
<log excerpt>
```

## Anything else?

<!-- Screenshots, stack traces, hunches, related issues. Optional. -->
