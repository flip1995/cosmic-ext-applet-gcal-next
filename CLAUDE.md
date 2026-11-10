# CLAUDE.md

## Version control

- Use **`jj`** (Jujutsu) for all version control operations, not `git`. The repo
  is colocated (`.jj/` next to `.git/`), so only fall back to raw `git` for
  things `jj` cannot do.
- Typical flow: `jj st`, `jj diff`, `jj describe -m "…"`, `jj new`,
  `jj file track <path>` for new files (auto-tracking is not enabled).
- Only create or describe commits when asked.

## Plan

The implementation plan lives in [`PLAN.md`](PLAN.md). Read it before starting
work, follow its phases, and keep it up to date (tick off checkboxes, record
decisions on the open questions) as the implementation progresses.
