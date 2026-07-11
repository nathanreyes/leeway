# Leeway — agent guide

## Plans

Design docs and implementation plans go in `knowledge/plans/`, **not**
`docs/plans/`. Name them `YYYY-MM-DD-<slug>-design.md`.

## Skills

Reusable, tool-neutral task guides live in `.agents/skills/<name>/SKILL.md`.
Each has YAML frontmatter (`name`, `description`) followed by step-by-step
instructions. Read the relevant `SKILL.md` in full before starting that task
and follow it exactly.

Claude Code auto-registers these as slash commands via symlinks in
`.claude/skills/`. Codex and other agents: read the files under
`.agents/skills/` directly.

Available skills:

- **release** (`.agents/skills/release/SKILL.md`) — cut a new Leeway release:
  bump the version, sync `Cargo.lock`, commit, tag, push, and watch the GitHub
  Release + crates.io publish workflows to completion. Follow when asked to
  release, ship, cut, tag, or publish a new version.
