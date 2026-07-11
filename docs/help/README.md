# Help content

These Markdown files are the source of the in-app help overlay (press `h` on any
focusable box). Each file is one help topic. They are embedded into the binary at
compile time via `include_str!` in `src/help.rs`, so **editing a file takes effect
after the next `cargo build`/`cargo run`** — there's no separate docs deploy.

## Format

The parser (`help::parse` in `src/help.rs`) understands a small subset of Markdown:

- `# Title` — the topic title shown in the modal's border (first `#` only).
- `## Heading` — a section heading (e.g. `How it fits`, `Example`, `Keys`).
- Paragraphs — normal prose; blank lines separate paragraphs. Rendered
  word-wrapped and reflowed, so hard line breaks within a paragraph don't matter.
- Fenced code blocks (```` ``` ````) — rendered verbatim (monospace, no wrapping).
  Use these for worked examples and keep lines short; leading spaces are preserved.
- Key list — bullet items of the form `` - `key` — description ``. Consecutive
  bullets are grouped into a key reference table. The backtick-quoted token becomes
  the highlighted key pill; everything after it (past the dash) is the description.

## Adding a topic

Content files map to `HelpTopic` variants in `src/help.rs`. To add a topic, add the
variant, a `topic_for`/`screen_ring` mapping, and an `include_str!` arm in
`markdown()` pointing at the new file here.
