# Leeway

Leeway is a local-first terminal budgeting app focused on one question:
**what's left?**

It is a forecasting tool, not an accounting ledger. You keep your current
checking balance as the ground truth, toggle known income and bills when they
settle, and let spending envelopes draw down over the month. The goal is a
low-friction monthly budget you can trust without entering every transaction.

Leeway is early open source software. The current first-run experience seeds
a starter plan, accounts, and month so you can explore the app immediately; that
onboarding flow will change as the project matures.

## Features

- Local SQLite storage with embedded migrations.
- Terminal UI built with Ratatui.
- Cash-flow dashboard centered on "what's left".
- Checking and credit-card account summaries.
- Reusable budget plans that can be stamped into monthly snapshots.
- Income, expense, and envelope budget blocks.
- Automatic envelopes that release budget over time.
- Manual envelopes for spending you want to enter directly.
- Shared series identities for recurring items and long-term trends.
- Restamping support for refreshing a month from a plan.

## Installation

Leeway currently ships from source. You need a Rust toolchain installed; the
standard route is [rustup](https://rustup.rs/).

```sh
git clone https://github.com/nathanreyes/leeway.git
cd leeway
cargo run
```

To install the local checkout as a command:

```sh
cargo install --path .
leeway
```

On first run, Leeway creates `leeway.db` in the current working directory,
applies the database schema, seeds starter data if the database is empty, and
opens the dashboard for the current month.

## Future Distribution

The long-term goal is for users not to need a source checkout.

- Rust users should be able to install from crates.io with
  `cargo install leeway`.
- Most users should be able to download prebuilt macOS, Linux, and Windows
  binaries from GitHub Releases.
- Package-manager installs, such as Homebrew, can come after the release process
  is stable.

Tools like [`cargo install`](https://doc.rust-lang.org/cargo/commands/cargo-install.html),
[crates.io publishing](https://doc.rust-lang.org/cargo/reference/publishing.html),
and [`cargo-dist`](https://axodotdev.github.io/cargo-dist/) are the likely path:
publish the crate for Rust-native installs, then automate release artifacts and
installers from tagged versions.

## Basic Usage

Run the app:

```sh
leeway
```

The dashboard is the daily loop. It shows account balances, remaining income,
remaining bills, envelopes, and the final "what's left" rollup.

Common dashboard keys:

- `Tab` / `Shift+Tab`: move between panels.
- `j` / `k` or arrow keys: move within the focused panel.
- `Enter`: perform the focused row's primary action; opens the selected envelope.
- `Space`: perform the focused row's lightweight action without leaving the dashboard.
- `n`: add an item to the focused panel.
- `l`: edit a label.
- `a`: edit an amount.
- `s`: record a transaction in the selected envelope.
- `P`: open Plans.
- `S`: open Series and trends.
- `q` / `Esc`: quit.

Plans are reusable templates. Stamp a plan into a month to create an independent
monthly snapshot. Editing a plan later does not rewrite past months.

Series are the durable identities behind recurring budget items. They let
Leeway connect "Rent" or "Groceries" across months and plans even when labels
or plan amounts change.

## Data

Leeway stores data in a local SQLite database named `leeway.db` by default.
The file is ignored by git and stays on your machine. Money values are stored as
integer cents, and the app derives the dashboard totals from the stored account,
transaction, envelope, plan, month, and series records.

## Documentation

Detailed usage and feature documentation will live in `/docs`. The existing
files there are design notes for current and planned behavior.

## Status

Leeway is pre-release. Expect the UI, onboarding, packaging, and docs to
change before a stable release.
