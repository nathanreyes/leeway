# Plans

Plans are simply a collection of income, bills, and envelopes. They are the baseline, ready to stamp into any month you choose, present or future.

Typically, you'll create a plan for your baseline month and then stamp it into each new month.

Your first plan is Main. Put everything you expect in it, and give items dates when they don't run forever.

To compare scenarios, like taking a vacation or buying a new car, make a what-if: Main with changes. A what-if follows Main except where you change it, so editing Main reaches every what-if. Stamp future months with a what-if to see how your budget changes.

## How it fits

Stamp a plan into a month to create an independent snapshot you can then adjust freely. Editing a plan afterwards doesn't touch months you've already stamped — a plan is the starting point, not a live link.

Editing a month's bills or envelopes doesn't change the plan either. You can use a plan to create a month, then tweak that month without affecting the plan.

## Stamp review

Press `s` from any budget panel to open the stamp window. The month pane checks the focused month, or next month when a plan is focused. The plan pane checks the focused plan, or starts empty when a month is focused. Use Space to check more months and plans. The item pane lists items from all checked plans. Use Space to turn an item off for this stamp, or Left/Right or `f` to choose static or a historical amount. `tab` switches panes; `enter` stamps every checked plan into every checked month.

Leeway remembers each amount source, but an off choice lasts for this stamp only. Seasonal months and dates still decide which checked months include each item; an item that sits out the previewed month says why, such as "starts Jan 2027". Items a checked what-if removes are named under the item pane. A stamped month and one plan must be selected alone so Leeway can ask how to restamp it.

Historical choices use settled bill and income amounts or spending recorded inside manual envelopes. Automatic envelopes stay static. If a saved source has no data for the target month, Leeway shows and uses the static amount but keeps the saved source for later.

## Restamping

Stamping a month that already has a plan restamps it. Unpaid items and envelopes match the plan's amounts again, and plan items the month lacks are added. Paid items stay paid at the amount you paid, and spending stays in its envelope.

Press `r` to restamp. If the month has items the plan doesn't, `x` restamps and removes them too: those bills, income, and envelopes with their spending. Removing a paid item doesn't change what's left.

## What-ifs

Press `w` to make a what-if on Main. It starts with no changes and stamps just as Main does. It shows under Main in the sidebar. Press `w` on a stamped month instead to save that month, edits and all, as a what-if.

Editing an item in a what-if changes it there only; Main keeps its own. Removing one of Main's items with `x` leaves it out of the what-if alone. Press `u` on a changed or removed item to make it follow Main again.

Rows say how the what-if differs from Main: a changed item shows Main's amount, an added item says "added", and a removed item stays in the list, struck through.

Restamping a month from a what-if takes out the unpaid items it removes. Paid items stay, and so do envelopes with spending.

Press `A` on a what-if to write its changes into Main. Leeway then deletes the what-if. Press `m` on a what-if to make it standalone: it keeps its items as they are and stops following Main. Main can't be deleted while it has what-ifs.

A plan that is neither Main nor a what-if, such as a small plan from before what-ifs, can join Main: press `m` to move its items into Main and delete it. Then give items dates where they need them.

## Dated items

Some items only run for a while: a car loan, a side job, one trip. Focus an item and press `D` to give it dates — `2027-01 to 2027-06`, `2027-01 to` for no end, `2027-08` for one month, or `none`.

Stamping skips months outside the dates, and the Summary leaves dated items out of a typical month.

## Seasonal items

Some money is just as regular but doesn't land every month: birthday gifts, school fees, an annual premium. Focus an item and press `M` to say which months it runs in — `mar,jul,nov`, `jun-aug`, or `all` to go back to every month.

Stamping then does the work. March gets the gift envelope; April doesn't. You don't have to remember, and months you stamp ahead show it too.

Because the months belong to the plan and not to the item itself, a leaner plan can drop the same gifts or move them.

Seasonal items stay out of the Summary totals, which describe an ordinary month. They're listed on their own line underneath, so you can see what a birthday month costs without it skewing the other nine.

## Example

```
  Plan: Monthly baseline
    Income      3,000
    Rent        1,500
    Internet       80
    Groceries     600 · automatic
    Dining        200 · manual
    Kid gifts     120 · Mar, Jul, Nov

  Stamp March -> the month opens with all of these.
  Stamp April -> the same, minus the gifts.
```

## Keys

Plans share the budget sidebar with months. The selected plan's Income, Expenses, Envelopes, and Summary fill the same detail area used by a month. `tab` moves focus between them; these keys act on the plan while the sidebar is focused.

- `j` / `k` — move through months and plans
- `n` — create a new plan
- `w` — create a what-if on Main
- `A` — apply the selected what-if to Main
- `m` — make the selected what-if standalone, or move another plan's items into Main
- `l` — rename the selected plan
- `s` — stamp the selected plan onto one or more months
- `x` — delete the selected plan (stamped months are kept)
- `tab` — move focus into the item panes

With an item pane focused:

- `a` — set this plan's amount for the item
- `M` — set which months the item runs in
- `D` — set the first and last month the item runs
- `u` — on a what-if, make a changed or removed item follow Main again
- `x` — remove the item from this plan (the series is kept); on a what-if, leave one of Main's items out
