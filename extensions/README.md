<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# extensions/

First-party **Steel** extensions, shipped with Majestic and version-pinned via the Nickel
manifest (Doom/`straight.el`-style reproducibility).

## How extension loading works (M2)

An extension is a Steel (`.scm`) script. You declare it in the Nickel manifest
(`majestic.ncl`), pinned to an exact version:

```nickel
{
  extensions = {
    example = { version = "0.1.0" },
    # `enabled = false` keeps the pin but skips loading; `source = "path.scm"`
    # overrides where the file is found (relative paths are from the manifest's dir).
  },
}
```

At startup `apply_config` loads each **enabled** extension (in name order) on the shared Steel
runtime, resolving its file to `<config-dir>/extensions/<name>.scm` unless `source` overrides it.
Every extension must declare its identity and version with `(majestic-provides! name version)`; the
loader verifies that against the manifest's pin and refuses a mismatch — that is what "pinned"
buys you (reproducibility). Loading is **fail-soft**: a missing file, an evaluation error, an
undeclared version, or a version mismatch surfaces a one-line status notice and the editor still
opens (run with `--safe` to skip configuration entirely).

Extensions share the runtime with each other and with your `config.scm`, so their registrations
are mutually visible. The M2 host API is intentionally small (settings overrides, `majestic-log`,
`majestic-provides!`); the full extension surface — commands, keymaps, hooks, UI, Oracle — and the
Seraph effect-table for agent-invoked code land at M2/M3.

See [`example.scm`](example.scm) for the minimal shape.
