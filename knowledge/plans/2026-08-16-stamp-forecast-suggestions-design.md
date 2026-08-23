# Stamp-time forecast suggestions

## The problem

A fixed plan amount is easy to trust but weak for bills and income that change each month.
Saving an ordered forecast policy would automate that work, but it would also add policy
editing, fallback ordering, and results that users might not see before stamping.

Leeway already has a review point: stamping turns a reusable plan into a fixed month
snapshot. Historical amounts belong there as choices, not as live links that can change an
existing month.

## The shape chosen

Each `plan_item` remembers one `forecast_method`:

- `static`
- `previous_month`
- `average_previous_3`
- `same_month_last_year`
- `overall_average`

Existing items default to `static`. The saved method has one fallback: the item's static
`amount_cents`. Missing history never blocks a stamp and never clears the saved method.

After the user enters a target month, stamping opens a review. It lists the concrete amount
for every active plan item. `j`/`k` moves between rows, Left/Right or `f` changes the source,
Enter saves the choices and continues, and Escape cancels. A restamp shows the same review
before the existing Merge/Replace choice.

The review keeps the common path short. A user who wants the plan unchanged can press
Enter once. Users only visit rows whose amount needs thought.

## Historical data rules

All queries exclude the target month and later months.

For standalone transaction series, a month supplies one observation only when every
matching occurrence is settled. The observation is the sum of those settled actuals.
This prevents an old forecast from becoming the input to a new forecast.

For manual envelope series, a month supplies one observation when the user recorded at
least one transaction inside a matching manual envelope. The observation is the sum of
that spending. A month with no recorded spending is missing, not zero, because Leeway does
not require full spending records.

Automatic envelopes offer only the static amount. Their consumed value comes from time,
not tracked spending, so it cannot support an actual-based forecast.

The methods have exact meanings:

- Previous month requires an observation in the prior calendar month.
- Average previous 3 requires observations in each of the prior three calendar months.
- Same month last year requires that exact calendar month.
- Overall average uses all earlier observations.
- Static always succeeds.

Daily manual envelopes normalize each historical monthly total by its source month length,
then expand it across the target month's day count. All averages round once to the nearest
minor currency unit.

## Repeated series

Leeway allows the same series more than once in a plan, but stamped rows carry only the
series id. Historical queries can recover a monthly total, not the share that belongs to
each plan occurrence. Applying the total to every occurrence would duplicate it.

When an active plan contains repeated occurrences of a series, each occurrence offers only
its static amount. This restriction can be removed if stamped rows later gain a durable
occurrence identity.

## Snapshot behavior

The resolver runs inside fresh stamp and both restamp paths. It returns a concrete amount;
the month stores that value in `amount_cents` and `stamped_amount_cents` as before. The
month does not keep a live link to the method or its source data.

Merge still protects settled transactions. Replace still resets matching rows. The
resolver excludes the target month, so restamping never uses the values it is about to
replace.

The Plans summary keeps using static amounts. A plan has no target month, so it cannot show
one sound historical result there. The stamp review owns the target-specific values.

## Schema and compatibility

Migration 004 adds `plan_item.forecast_method TEXT NOT NULL DEFAULT 'static'` with a check
for the five known values. The schema version rises to 4. Folder sync already rejects a
snapshot with a newer schema version, so an older app will fail closed.

## Tests

Tests cover:

- the static migration default;
- settled transaction history;
- rejection of unsettled history;
- exact three-calendar-month averages;
- same-month-last-year and overall averages;
- repeated-series protection;
- static-only automatic envelopes;
- saved review choices;
- fallback without loss of the saved choice; and
- review rendering.
