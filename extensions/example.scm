; SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
; SPDX-License-Identifier: GPL-3.0-or-later
;
; A minimal example Majestic extension.
;
; Extensions are Steel scripts loaded per the Nickel manifest, each pinned to an exact version
; (Doom/straight.el-style reproducibility). Enable it from `majestic.ncl`:
;
;   extensions = { example = { version = "0.1.0" } }
;
; Every extension MUST declare its name and version; the loader verifies this against the manifest
; pin and refuses a mismatch. The M2 host API is small (settings overrides, logging); commands,
; keymaps, hooks, UI, and Oracle registration land as the API surface grows (PRD #1 §5.5 / §6.7).

(majestic-provides! "example" "0.1.0")
(majestic-log "example extension loaded")
