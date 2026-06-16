<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Contributing to Majestic

Thank you for your interest. Please read this before opening an issue or pull request — it
sets honest expectations for both sides.

## Project Stance

Majestic is a **personal hobby project** under Spacecraft Software, shaped around the
maintainer's own use case and developed at hobby pace. This is **not** a community-driven
project, but external input is welcome within the bounds below.

## What Is Welcome

- **Bug reports** — clear, reproducible, with environment details (OS, kernel, Rust
  toolchain version, terminal, shell, relevant config).
- **Suggestions** — features, refactors, naming proposals (new codenames must be
  aerospace/astronomy or sci-fi/AI per the
  [Spacecraft Software Standard §2](https://Standard.SpacecraftSoftware.org/)), design feedback.
- **Pull requests** — small, focused, aligned with the Standard and the Product Contract
  (see [`MAJESTIC.md`](./MAJESTIC.md)).
- **Documentation fixes** and **test/coverage improvements** — almost always merge-worthy.

## What Is Not Guaranteed

- **PR acceptance.** Direction, scope, and the quality bar are set by the maintainer alone.
  A correct, well-written, CI-passing PR is still not a guaranteed merge. Rejection reflects
  fit, not quality.
- **Response time**, **roadmap influence**, and **pre-1.0 API stability** (the extension API
  and crate APIs may break in any release until 1.0).

## Before Opening a PR

1. **Open an issue first** for non-trivial changes; discuss the design before writing code.
2. **Read the Standard and the Product Contract.** Stability → Performance (designed-in
   concurrency) → Hardened Security, in that order. `#![deny(unsafe_code)]` workspace-wide.
   POSIX-compliant CLI surface.
3. **Run the gates locally** and make them green:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo audit          # for any added dependency
   reuse lint           # REUSE/SPDX compliance
   ```
4. **Add deterministic tests for any concurrent code** (Morpheus seeded test executor) —
   shipping a concurrency feature without them is a review blocker.
5. **Sign off your commits** (`git commit -s`) under the
   [Developer Certificate of Origin](https://developercertificate.org/).
6. **Sign your commits cryptographically** (Standard §6.3): Ed25519 SSH signing
   (`commit.gpgsign=true`, `gpg.format=ssh`, key registered as a *Signing* key); every commit
   must show **Verified** on the host. Rewrites must preserve signatures.

## Commit Style

- Conventional Commits prefix (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`,
  `perf:`, `build:`, `ci:`).
- Subject ≤ 72 chars, imperative mood ("add" not "added").
- Body wrapped at 72 columns; explain *why*. Reference issues (`Closes #42`).

## Reporting Security Issues

Do **not** open a public issue. Email &lt;Mohamed.Hammad@SpacecraftSoftware.org&gt; with
details. A coordinated-disclosure window of 90 days from acknowledgment is the default.
This matters especially for **Seraph** (the AI guardrail) — report any bypass privately.

## License of Contributions

By submitting a contribution you agree it will be licensed under **GPL-3.0-or-later** (code)
or **CC-BY-SA-4.0** (documentation), matching the file class. Contributions that cannot be
so licensed cannot be accepted. You retain copyright; no CLA is required.

---

**Maintainer:** Mohamed Hammad &lt;Mohamed.Hammad@SpacecraftSoftware.org&gt;
**License:** GPL-3.0-or-later
<https://Majestic.SpacecraftSoftware.org/>

*— Built by Spacecraft Software —*
