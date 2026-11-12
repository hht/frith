# CLAUDE.md — Frith

> The user's input in the current turn takes precedence over any document.

## What this is

A desktop GPU terminal and multiplexer for running AI coding agents (Claude Code, Codex, Cursor Agent, …) side by side: macOS first, then Windows and Linux. Product goals: small binary, excellent performance, full TUI support, and a striking UI (in-window frosted-glass panes, custom shaders). Plugins run out of process over stdio JSON-RPC and contribute data only; the host draws all UI. The design is original: draw on the category, not on any one product's screens.

The stack is under evaluation: Rust + winit + skia-safe (Ganesh) + libghostty-vt, pending the spike in `spike-skia-ghostty/`. Facts and risks: [research](docs/research/skia-libghostty-terminal.md).

## Core principles

**Highest rule: blast radius before every fix.** Before writing a fix, list every caller, consumer, persisted state, invariant, and documented contract it touches, and state how each one's behavior changes; cover each that could break with a test, and stop for the user when one changes in a way they have not approved.

1. **Deletion first.** Every line must answer "what breaks if this is deleted?" If you cannot answer, trace its origin; if you still cannot answer after tracing, delete it. Inline pass-through or renaming-only functions, types, and files; write code only for requested needs (tests and refactoring excluded).
2. **One module, one question.** Divide modules by question (one design decision that changes independently), never by processing step; keep interfaces much smaller than implementations, and state at the top which question the module answers and what it must not know. Keep dependencies unidirectional and acyclic; keep the core (layout, terminal state handling, plugin protocol) pure and directly testable, with window, GPU, PTY, and IPC at the edges.
3. **Measure.** Performance is the product: judge render and parse work by frame time p50/p99, throughput, and binary size measured on a release build, and choose data structures for the scale up front (many panes, long scrollback, agents streaming output).
4. **Trust boundaries.** PTY output, plugin messages, clipboard requests (OSC 52), and config are untrusted: parse them into exact types at the boundary and use those types inside. Reserve fallbacks and defaults for cases that genuinely occur; panic on broken internal invariants and report at the top level.
5. **Fix at the root.** Change code where the problem originates; if you cannot find the root cause, stop and report. Write a failing test before fixing a bug.
6. **Primary sources.** Follow mature terminals (Ghostty, Alacritty, WezTerm, kitty) and Neovide for Skia rendering, writing only deviations the platform forces; verify crate and library behavior against the exact pinned version's source, and cite official docs or version-tagged source with `@see` in comments.

Hold the product to four properties: TUIs behave exactly as in a reference terminal, input never waits on rendering or IO, the binary stays small, and idle panes cost nothing.

## Layout

```
docs/research/        Research notes from the `research` skill, with sources
spike-*/              Throwaway prototypes answering one question each; never imported by product code
```

## Documentation

Code and tests are the source of truth. Documentation records only user-approved decisions and rationale not visible from the code; record each approved architecture decision in `docs/adr.md` (create it with the first one) and ask the user before changing one. Write identifiers and code comments in simple English; write `docs/` in Chinese.

Adding a dependency or a tool requires explicit user approval.

## Environment

- Install tools with mise, pinned in the nearest `mise.toml`, and run builds through it (`mise exec -- cargo …`); use Homebrew only when the user approves.
- libghostty-vt builds only with Zig 0.17.x; any other minor version fails the build.
- mise installs that hit the GitHub API can fail with 403 from rate limiting; retry with `GITHUB_TOKEN=$(gh auth token)`.
- A spike's run knobs live in the header comment of its `src/main.rs`.

## Commits

- Follow Conventional Commits in English, types `feat` `fix` `change` `refactor` `test` `docs` `deps` `chore`, no scope.
- Each commit makes one self-contained change with its tests; keep refactoring separate from features and fixes.
- Before committing, review the diff line by line, deletion first, and report what was deleted, changed, and kept with justification; then dispatch a fresh read-only subagent (not a fork) to review the diff for regressions and bugs, citing `file:line` or a reproduction for each finding.

## Reporting

- Summarize first, then suggest next steps.
- Every decision left to the user comes with a comparison table of the options (what each changes, cost, risk, which of the four properties it affects) and a recommendation with its reason.
- Back every conclusion with evidence (source `file:line`, official documentation link, or measurement); mark unverified inferences as "inferred / unverified". Point out flawed premises directly.

## Sessions

`HANDOFF.md` at the repo root is the primary hand-off document: a new session resumes from it alone. Read it first when a session starts. Watch the context: when it nears 300k tokens, finish the current step, then run `handoff` (overwrites `HANDOFF.md`, never committed) before continuing.

## Agent skills

| Intent | Skill | Output location |
|---|---|---|
| Refine designs and resolve open questions | `grilling` | `docs/adr.md` |
| Research library or protocol facts | `research` | `docs/research/` |
| Answer a design or performance question with throwaway code | `prototype` | `spike-<topic>/` |
| Implement features | `tdd` (interface vocabulary in `codebase-design`) | Code and tests |
| Diagnose hard bugs or slowness | `diagnosing-bugs` | Root cause in commit message |
| Edit `CLAUDE.md` or skills | `writing-for-agents` | — |
| Hand off between sessions | `handoff` | `HANDOFF.md` at repo root |
