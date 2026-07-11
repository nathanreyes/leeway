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

### Prebuilt binary (recommended)

Leeway ships self-contained binaries for macOS, Linux, and Windows from
[GitHub Releases](https://github.com/nathanreyes/leeway/releases). The install
script downloads the right build for your platform and puts `leeway` on your
`PATH`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nathanreyes/leeway/releases/latest/download/leeway-installer.sh | sh
```

You can also grab an archive for your platform directly from the
[latest release](https://github.com/nathanreyes/leeway/releases/latest) and
extract the `leeway` binary yourself. Each archive ships with a `.sha256`
checksum.

### From crates.io

If you have a [Rust toolchain](https://rustup.rs/) installed:

```sh
cargo install leeway
```

### From source

```sh
git clone https://github.com/nathanreyes/leeway.git
cd leeway
cargo run          # run in place
cargo install --path .   # or install the checkout as a command
```

On first run, Leeway creates `leeway.db` in the current working directory,
applies the database schema, seeds starter data if the database is empty, and
opens the dashboard for the current month.

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
- `l`: edit a local label where available; recurring item names are edited from Series (`S`).
- `a`: edit an amount.
- `s`: record a transaction in the selected envelope.
- `P`: open Plans.
- `S`: open the selected series and trends; press again for the full Series list.
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
