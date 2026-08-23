//! In-app contextual help.
//!
//! Pressing `h` on any focusable box opens a help overlay describing what that
//! content is, how it fits Leeway's "what's left" forecasting model, a worked
//! example, and the keys that act on it. There is no separate docs site: the
//! explanation lives next to the thing it explains.
//!
//! The prose itself lives in Markdown files under `docs/help/`, embedded at
//! compile time with `include_str!` (see `markdown`) and parsed by `parse` into
//! the section model below. Edit a `.md` file and rebuild to change the help.
//!
//! This module also maps the app's current focus to a topic (`topic_for`), the
//! ring of sibling topics reachable on a screen (`screen_ring`), and renders a
//! topic's sections to pre-wrapped `Line`s (`lines`). `main.rs` owns the modal
//! shell that frames it.

use std::cell::Cell;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::{App, BudgetTarget, DashFocus, PlanFocus, Screen, theme};

/// Every focusable region across the app that has help, plus the app-level
/// `Overview`. Each variant maps to one Markdown file in `markdown()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpTopic {
    Overview,
    DashHeader,
    DashIncome,
    DashExpenses,
    DashEnvelopes,
    DashAccounts,
    PlanIncome,
    PlanExpenses,
    PlanEnvelopes,
    Series,
    PlansList,
}

/// The authored help for one topic: a title and an ordered list of sections.
/// Parsed from the topic's Markdown file (see `content`), so the fields are owned.
pub struct HelpContent {
    pub title: String,
    pub sections: Vec<HelpSection>,
}

/// A block within a topic. `Para` is prose (word-wrapped at render time);
/// `Heading` labels a section; `Example` is monospace lines shown verbatim;
/// `Keys` is a key/description reference table.
pub enum HelpSection {
    Para(String),
    Heading(String),
    Example(Vec<String>),
    Keys(Vec<(String, String)>),
}

/// Live state for an open help modal: the sibling ring for the screen `h` was
/// pressed on, the topic currently shown (which may be `Overview`, outside the
/// ring), the vertical scroll offset, and the last-rendered maximum scroll so
/// the key handler can clamp without knowing the frame size.
pub struct HelpState {
    pub ring: Vec<HelpTopic>,
    pub current: HelpTopic,
    pub scroll: u16,
    pub max_scroll: Cell<u16>,
}

impl HelpState {
    pub fn new(app: &App) -> Self {
        HelpState {
            ring: screen_ring(app),
            current: topic_for(app),
            scroll: 0,
            max_scroll: Cell::new(0),
        }
    }

    /// Move to the next/previous sibling topic. When on `Overview` (outside the
    /// ring), step back into the ring at its start. Resets scroll.
    pub fn cycle(&mut self, forward: bool) {
        if self.ring.is_empty() {
            return;
        }
        let pos = self.ring.iter().position(|&t| t == self.current);
        let next = match pos {
            Some(i) if forward => (i + 1) % self.ring.len(),
            Some(i) => (i + self.ring.len() - 1) % self.ring.len(),
            None => 0, // currently on Overview → drop into the ring
        };
        self.current = self.ring[next];
        self.scroll = 0;
    }

    pub fn show_overview(&mut self) {
        self.current = HelpTopic::Overview;
        self.scroll = 0;
    }
}

/// Resolve the topic for the box the user is currently focused on.
pub fn topic_for(app: &App) -> HelpTopic {
    match &app.screen {
        Screen::Budget => match app.budget_target {
            BudgetTarget::Month { .. } => match app.dash_focus {
                DashFocus::Header => HelpTopic::DashHeader,
                DashFocus::Income => HelpTopic::DashIncome,
                DashFocus::Expenses => HelpTopic::DashExpenses,
                DashFocus::Envelopes => HelpTopic::DashEnvelopes,
                DashFocus::Accounts => HelpTopic::DashAccounts,
            },
            BudgetTarget::Plan { .. } => match app.plan_focus {
                PlanFocus::List => HelpTopic::PlansList,
                PlanFocus::Income => HelpTopic::PlanIncome,
                PlanFocus::Expenses => HelpTopic::PlanExpenses,
                PlanFocus::Envelopes => HelpTopic::PlanEnvelopes,
            },
        },
        Screen::Series { .. } => HelpTopic::Series,
        Screen::Settings { .. } => HelpTopic::Overview,
    }
}

/// The ordered sibling topics reachable on the current screen, used for in-modal
/// `Tab` cycling. Mirrors each screen's own focus cycle so "next topic" matches
/// "next panel". The dashboard omits Accounts off-month, where that panel is
/// hidden and focus can't land on it.
pub fn screen_ring(app: &App) -> Vec<HelpTopic> {
    match &app.screen {
        Screen::Budget => match app.budget_target {
            BudgetTarget::Month { .. } => {
                let mut ring = vec![
                    HelpTopic::DashHeader,
                    HelpTopic::DashIncome,
                    HelpTopic::DashExpenses,
                    HelpTopic::DashEnvelopes,
                ];
                if app.dash_focus == DashFocus::Accounts {
                    ring.push(HelpTopic::DashAccounts);
                }
                ring
            }
            BudgetTarget::Plan { .. } => vec![
                HelpTopic::PlansList,
                HelpTopic::PlanIncome,
                HelpTopic::PlanExpenses,
                HelpTopic::PlanEnvelopes,
            ],
        },
        Screen::Series { .. } => vec![HelpTopic::Series],
        Screen::Settings { .. } => vec![HelpTopic::Overview],
    }
}

/// The raw Markdown for a topic, embedded at compile time. This match is the
/// single registry tying `HelpTopic` variants to `docs/help/*.md` files.
fn markdown(topic: HelpTopic) -> &'static str {
    match topic {
        HelpTopic::Overview => include_str!("../docs/help/overview.md"),
        HelpTopic::DashHeader => include_str!("../docs/help/dash-header.md"),
        HelpTopic::DashIncome => include_str!("../docs/help/dash-income.md"),
        HelpTopic::DashExpenses => include_str!("../docs/help/dash-expenses.md"),
        HelpTopic::DashEnvelopes => include_str!("../docs/help/dash-envelopes.md"),
        HelpTopic::DashAccounts => include_str!("../docs/help/dash-accounts.md"),
        HelpTopic::PlanIncome => include_str!("../docs/help/plan-income.md"),
        HelpTopic::PlanExpenses => include_str!("../docs/help/plan-expenses.md"),
        HelpTopic::PlanEnvelopes => include_str!("../docs/help/plan-envelopes.md"),
        HelpTopic::Series => include_str!("../docs/help/series.md"),
        HelpTopic::PlansList => include_str!("../docs/help/plans.md"),
    }
}

/// The authored help for a topic, parsed from its embedded Markdown file.
pub fn content(topic: HelpTopic) -> HelpContent {
    parse(markdown(topic))
}

/// Parse the small Markdown subset used by the help files into sections:
///   `# Title`   → the topic title (first one wins)
///   `## Head`   → a section heading
///   ```` ``` ```` fenced block → an `Example` (verbatim lines)
///   `- `key` — desc`  runs of bullets → a `Keys` table
///   anything else → prose paragraphs, split on blank lines
pub fn parse(md: &str) -> HelpContent {
    let mut title = String::new();
    let mut sections: Vec<HelpSection> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();

    let mut iter = md.lines().peekable();
    while let Some(line) = iter.next() {
        let trimmed = line.trim();

        // Fenced code block → verbatim example. Collect until the closing fence.
        if trimmed.starts_with("```") {
            flush_paragraph(&mut paragraph, &mut sections);
            let mut example: Vec<String> = Vec::new();
            for body in iter.by_ref() {
                if body.trim().starts_with("```") {
                    break;
                }
                example.push(body.to_string());
            }
            sections.push(HelpSection::Example(example));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush_paragraph(&mut paragraph, &mut sections);
            if title.is_empty() {
                title = rest.trim().to_string();
            } else {
                sections.push(HelpSection::Heading(rest.trim().to_string()));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush_paragraph(&mut paragraph, &mut sections);
            sections.push(HelpSection::Heading(rest.trim().to_string()));
            continue;
        }

        // A run of `- ...` bullets becomes one Keys table.
        if trimmed.starts_with("- ") {
            flush_paragraph(&mut paragraph, &mut sections);
            let mut keys: Vec<(String, String)> = Vec::new();
            let mut current = Some(line);
            while let Some(l) = current {
                let item = l.trim().strip_prefix("- ").unwrap_or("").trim();
                if let Some(entry) = parse_key_entry(item) {
                    keys.push(entry);
                }
                match iter.peek() {
                    Some(next) if next.trim().starts_with("- ") => current = iter.next(),
                    _ => current = None,
                }
            }
            sections.push(HelpSection::Keys(keys));
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut sections);
        } else {
            paragraph.push(trimmed.to_string());
        }
    }
    flush_paragraph(&mut paragraph, &mut sections);

    HelpContent { title, sections }
}

/// Join a pending paragraph's lines and push it as a `Para` section, if any.
fn flush_paragraph(paragraph: &mut Vec<String>, sections: &mut Vec<HelpSection>) {
    if !paragraph.is_empty() {
        sections.push(HelpSection::Para(paragraph.join(" ")));
        paragraph.clear();
    }
}

/// Parse one key-list item — `` `key` — description `` — into (key, description).
/// The key is the first backtick-quoted token; the description is the remainder
/// with any leading dash/space/colon separators trimmed off.
fn parse_key_entry(item: &str) -> Option<(String, String)> {
    let rest = item.trim().strip_prefix('`')?;
    let end = rest.find('`')?;
    let key = rest[..end].to_string();
    let desc = rest[end + 1..]
        .trim_start_matches([' ', '—', '-', ':'])
        .trim()
        .to_string();
    Some((key, desc))
}

/// A small pill-styled key hint, matching the footers' `key()` spans. Pads the
/// label so the highlight reads as a button regardless of the source text.
fn pill(label: &str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default().fg(Color::Black).bg(Color::Gray),
    )
}

/// Word-wrap `text` to `width` columns. Words longer than `width` overflow their
/// own line rather than being split mid-word.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Render a topic's sections into fully pre-wrapped lines for `width` columns.
/// Because wrapping happens here, the returned count is the exact rendered height
/// — the caller uses it to clamp scrolling without asking ratatui to re-wrap.
pub fn lines(content: &HelpContent, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let gray = Style::default().fg(Color::Gray);
    let mut out: Vec<Line> = Vec::new();

    for (i, section) in content.sections.iter().enumerate() {
        match section {
            HelpSection::Heading(text) => {
                if i != 0 {
                    out.push(Line::raw(""));
                }
                out.push(Line::from(Span::styled(
                    format!(" {text}"),
                    Style::default()
                        .fg(theme::MAUVE)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            HelpSection::Para(text) => {
                // Reserve a one-column left margin so prose doesn't touch the border.
                for wrapped in wrap(text, width.saturating_sub(1)) {
                    out.push(Line::from(Span::styled(format!(" {wrapped}"), gray)));
                }
            }
            HelpSection::Example(rows) => {
                for row in rows {
                    out.push(Line::from(Span::styled(
                        row.clone(),
                        Style::default().fg(theme::CYAN),
                    )));
                }
            }
            HelpSection::Keys(rows) => {
                for (k, desc) in rows {
                    out.push(Line::from(vec![
                        Span::raw(" "),
                        pill(k),
                        Span::styled(format!("  {desc}"), gray),
                    ]));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TOPICS: &[HelpTopic] = &[
        HelpTopic::Overview,
        HelpTopic::DashHeader,
        HelpTopic::DashIncome,
        HelpTopic::DashExpenses,
        HelpTopic::DashEnvelopes,
        HelpTopic::DashAccounts,
        HelpTopic::PlanIncome,
        HelpTopic::PlanExpenses,
        HelpTopic::PlanEnvelopes,
        HelpTopic::Series,
        HelpTopic::PlansList,
    ];

    #[test]
    fn every_topic_parses_to_a_titled_document() {
        // Guards against a missing file, a bad include_str! path, or an empty doc.
        for &topic in ALL_TOPICS {
            let content = content(topic);
            assert!(!content.title.is_empty(), "topic {topic:?} has no title");
            assert!(
                !content.sections.is_empty(),
                "topic {topic:?} has no sections"
            );
        }
    }

    #[test]
    fn parse_recognizes_each_section_kind() {
        let md = "# Envelopes\n\
                  \n\
                  Intro line one\n\
                  and line two.\n\
                  \n\
                  ## Example\n\
                  \n\
                  ```\n\
                  Groceries 600\n\
                  ```\n\
                  \n\
                  ## Keys\n\
                  \n\
                  - `enter` — open detail\n\
                  - `s` — record spending\n";
        let content = parse(md);
        assert_eq!(content.title, "Envelopes");

        // Order: intro Para, "Example" Heading, Example block, "Keys" Heading, Keys.
        assert!(
            matches!(&content.sections[0], HelpSection::Para(p) if p == "Intro line one and line two.")
        );
        assert!(matches!(&content.sections[1], HelpSection::Heading(h) if h == "Example"));
        assert!(
            matches!(&content.sections[2], HelpSection::Example(rows) if rows == &["Groceries 600"])
        );
        assert!(matches!(&content.sections[3], HelpSection::Heading(h) if h == "Keys"));
        match &content.sections[4] {
            HelpSection::Keys(rows) => {
                assert_eq!(rows[0], ("enter".to_string(), "open detail".to_string()));
                assert_eq!(rows[1], ("s".to_string(), "record spending".to_string()));
            }
            _ => panic!("expected a keys table"),
        }
    }
}
