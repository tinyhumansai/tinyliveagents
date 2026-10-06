# Repository Guidelines

This file is the single source of truth for how humans and coding agents work
in this repository. `CLAUDE.md` is a symlink to this file, so every agent reads
the same instructions.

## Project Structure

A Rust 2024 cargo workspace rooted at a virtual `Cargo.toml`. Every crate lives
under `crates/`, one directory per package.

```text
Cargo.toml                  # virtual workspace: members, default-members,
                            # [workspace.package], [workspace.dependencies], lints
clippy.toml                 # doc-valid-idents for product names
crates/
├── tinyliveagents/         # the library (the only default member)
│   └── src/
│       ├── lib.rs          # crate docs + the whole public surface
│       ├── error/          # crate-wide `Error` and `Result<T>`
│       ├── types/          # the standard vocabulary (config, commands, events)
│       ├── session/        # LiveSession / LiveSender / LiveEvents
│       ├── provider/       # the LiveProvider trait + shared config checks
│       ├── transport/      # WebSocket connect and the generic `WireCodec` driver
│       ├── audio/          # PCM16 helpers
│       ├── testkit/        # cfg(test): mock WebSocket / HTTP servers
│       ├── gemini/         # feature `gemini`
│       ├── elevenlabs/     # feature `elevenlabs`
│       └── sarvam/         # feature `sarvam`
└── tinyliveagents-examples/  # publish = false: examples + live network tests
docs/
├── specs/                  # behavior and architecture specifications
├── plans/                  # test-first implementation plans
└── adr/                    # immutable architecture decision records
```

### What belongs here

This crate **standardizes live voice provider APIs and nothing else**. A
change belongs here when it is about how a provider's wire protocol maps onto
the standard vocabulary. It does not belong here when it executes tools,
stores or resolves credentials, mints tickets or signed URLs, applies approval
or product policy, or persists transcripts: those live in the host or in an
agent harness (OpenHuman uses `tinyagents-live`).

### Adding a provider

1. Add a feature in `crates/tinyliveagents/Cargo.toml` and a module gated on
   it in `lib.rs`.
2. Single-socket providers implement `transport::WireCodec` (pure
   encode/decode) and call `transport::drive`; anything more (like the Sarvam
   cascade) owns its task, but must still honour the contract in
   `docs/specs/live-session.md`.
3. Test the codec frame by frame and the provider end to end against
   `testkit::MockServer`; add a live test and an example to
   `tinyliveagents-examples`.
4. Add a row to the provider tables in `README.md`, `lib.rs` and the spec.

Each feature area is a directory module. A module root explains the module,
wires its pieces together, and exposes the smallest useful API. Keep public
exports centralized in `src/lib.rs`.

## Build And Test

Run every command from the repository root. These four are the contract; CI
runs exactly them, so a green local run should mean a green CI run.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Supporting commands:

- `cargo fmt --all` — format before committing.
- `cargo test <filter>` — run a focused subset while iterating.
- `cargo test -p tinyliveagents` — run the library suite.
- `cargo test -p tinyliveagents-examples --test live -- --ignored` — run the
  live tests (each skips when its key is unset; see `.env.example`).
- `.github/scripts/check-file-coverage.sh 90 coverage.json` — the per-file
  coverage gate CI runs.
- `cargo doc --no-deps --all-features` — build the rustdoc CI also builds with
  `RUSTDOCFLAGS="-D warnings"`.
- `cargo test --doc` — run doctests alone when editing documentation examples.

Never skip, ignore, or delete a failing test to make a command pass. Fix the
root cause, or stop and report the blocker.

## Coding Style

Use standard `rustfmt` output and Rust 2024 idioms. Do not hand-format around
`rustfmt`, and do not add `#[rustfmt::skip]` without a comment explaining why.

- `snake_case` for modules, files, functions, methods, fields, and locals.
- `PascalCase` for types, traits, and enum variants; `SCREAMING_SNAKE_CASE` for
  constants and statics.
- Name things for what they are, not for their layer: `RetryPolicy`, not
  `RetryHelper`.
- Prefer small, typed APIs over stringly-typed ones. Accept `&str` and generic
  `impl Into<String>` at boundaries; return owned, concrete types.
- Keep the public surface minimal: default to private, and export deliberately
  from `src/lib.rs`.
- `unsafe` is forbidden workspace-wide by `[workspace.lints]` in the root
  `Cargo.toml`. If a project genuinely needs it, relax the lint in its own
  commit and document every invariant with a `// SAFETY:` comment.

### Errors

- One crate-wide `Error` enum per crate, in `src/error/mod.rs`, built with
  `thiserror`.
- Fallible public functions return `Result<T>`, the crate alias.
- Add a specific variant instead of stuffing context into a string; error
  messages are lowercase, without trailing punctuation.
- Do not `unwrap()`, `expect()`, or `panic!` in library code paths. They are
  fine in tests, examples, and genuinely unreachable states — where `expect`
  must carry a message explaining the invariant.
- Document a `# Errors` section on every public fallible function and a
  `# Panics` section on anything that can panic.

### Dependencies

Adding a dependency is a design decision. Before adding one, check whether the
standard library or an existing dependency already covers the need. When you do
add one:

- pin a caret range (`serde = "1"`), not an exact version;
- enable only the features you need, with `default-features = false` when that
  meaningfully trims the tree;
- gate anything optional behind a Cargo feature, documented in `Cargo.toml`;
- declare it once in the root `[workspace.dependencies]` when more than one
  crate needs it, and take it with `{ workspace = true }`;
- leave a comment above the entry explaining *why* the crate is needed and what
  uses it — see the existing entries for the expected tone;
- prefer well-maintained crates with a compatible license.

Keep `Cargo.lock` committed; this workspace ships a single lockfile so CI and
releases are reproducible.

## Testing

- Module-local unit tests live in `crates/<crate>/src/<feature>/<module>_tests.rs`
  (`mod_tests.rs` beside a `mod.rs`) and may touch private items. Never name
  one `test.rs` or `tests.rs`, and never write an inline `mod tests { ... }`:
  OpenHuman vendors this crate and enforces the same layout.
- Provider tests run against `testkit::MockServer` / `testkit::MockHttp`, never
  the network. Live tests go in `crates/tinyliveagents-examples/tests/live.rs`,
  `#[ignore]`d and skipping when their key is unset.
- Integration tests live in `crates/<crate>/tests/` and exercise only the public
  API — they are the regression suite for the crate's contract.
- Payload types pin their serde representation in a unit test. That
  representation is the wire form: a host and a module that disagree about a
  field name fail at runtime with a decode error.
- Use descriptive, behavioral test names: `rejects_an_empty_name`, not
  `test_greet_2`.
- Cover the failure paths, not just the happy path. Every new error variant
  needs a test that produces it.
- For async behavior, standardize on one runtime (`tokio` as a dev-dependency
  for tests) rather than mixing runtimes.
- Tests must be deterministic and independent of network, wall-clock time, and
  execution order. Gate any live/network test behind a feature or an env var and
  name it `live_*` so it is easy to exclude.
- Maintain at least 90% line coverage in every source file. Add or update tests
  with every behavior change, and note any deliberately untested edge case in
  the pull request description.

Write the test first when fixing a bug: a failing test that reproduces the
report, then the fix that turns it green.

## Documentation

Write documentation for the reader who has never seen the code.

- Every public item gets a rustdoc comment. `missing_docs` is a warning that CI
  treats as an error.
- Start every `mod.rs` and `*_tests.rs` with a concise module-level `//!`
  description.
- Each crate's `src/lib.rs` carries its crate-level overview: what the crate
  does, the primary entry points, and a short runnable example. It should also
  say what the crate deliberately does *not* hold, and why.
- Prefer concrete examples over vague description. Doc examples are compiled and
  run by `cargo test`, so they cannot drift.
- Complex modules must include a module-level `README.md` covering their design,
  public surface, and important operational constraints.
- Keep `README.md`, `docs/`, and module docs aligned with code changes in the
  same commit that changes behavior.
- Write accepted behavior and constraints in `docs/specs/` before creating a
  linked, implementation-ordered plan in `docs/plans/`. Specs define what and
  why; plans define how and in what sequence.
- Keep every Markdown file, including this one, at 500 lines or fewer. When a
  topic outgrows that, split it into focused files and link them from the
  nearest `README.md`.

## Git Workflow

- Never commit directly to `main`. Branch first, one branch per logical change.
- Do feature work in a git worktree so the main checkout stays clean.
- Commit subjects are concise and imperative: `Add retry policy to the client`.
  Keep the subject specific to the change and under ~72 characters.
- Make small, focused commits. Each commit should cover one logical change,
  build independently, and avoid mixing formatting, refactors, and behavior
  changes unless they are inseparable.
- Never commit secrets. `.env` is git-ignored; document new variables in
  `.env.example` with placeholder values.
- Never force-push a shared branch, rewrite published history, or bypass hooks
  with `--no-verify`.

## Pull Requests

Open pull requests ready for review, not as drafts, unless the work genuinely
must not merge yet. A pull request should:

- summarize what changed and why, in a few sentences;
- call out public API or behavior changes explicitly, or state "None";
- list the validation commands actually run, with their outcome;
- link the related issue;
- include updated tests, docs, and examples in the same change.

The template in `.github/PULL_REQUEST_TEMPLATE.md` encodes this checklist.
Address review feedback by fixing it, and reply on each thread describing what
changed. Do not resolve a thread whose feedback you have not addressed or
explicitly declined with a reason.

## Releases

Releases run from `.github/workflows/release.yml` via a manual
`workflow_dispatch` with a `patch` / `minor` / `major` bump; `current` resumes
an interrupted release after its version commit and tag exist. The workflow
re-runs the validation suite, bumps the root `[workspace.package]` version and
`Cargo.lock`, commits and tags `vX.Y.Z`, and creates a GitHub release. There
are no binary artifacts: consumers (OpenHuman, through `tinyagents`) pin the
tag as a git dependency.

- Do not hand-edit the `version` field in the root `[workspace.package]`; the
  release workflow owns it.
- Follow semantic versioning. Pre-1.0, any change to the public surface that is
  not purely additive needs a minor bump.

## Agent Working Agreement

For automated contributors specifically:

1. **Read before writing.** Inspect the surrounding module and match its
   conventions, comment density, and idiom rather than importing a house style.
2. **Verify, do not assume.** Run the four contract commands and read their
   output before reporting a task complete. Report failures with the output;
   never claim a check passed that you did not run.
3. **Stay in scope.** Implement what was asked. Do not opportunistically
   refactor, reformat, upgrade dependencies, or "fix" unrelated code — raise it
   instead.
4. **No placeholders in delivered code.** No `todo!()`, no stubbed functions, no
   commented-out alternatives left behind. If something cannot be finished, say
   so explicitly.
5. **Do not weaken the guardrails.** Never add blanket `#[allow(...)]`, relax a
   lint, mark a test `#[ignore]`, or loosen CI to get a green run. Fix the
   cause.
6. **Secrets stay out.** Never read, echo, or commit `.env` contents, tokens, or
   credentials, and never paste them into a pull request or issue.
7. **Ask only when blocked.** Make routine judgment calls yourself; escalate
   only irreversible decisions or genuine forks with no clear default.
