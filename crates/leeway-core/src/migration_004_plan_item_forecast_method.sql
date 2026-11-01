-- Leeway schema — v4: one remembered forecast source per plan item.
--
-- Static remains the default, so existing plans stamp exactly as before. Historical
-- methods resolve when a month is stamped. If the requested history is missing, stamping
-- uses amount_cents instead without changing this saved choice.
ALTER TABLE plan_item ADD COLUMN forecast_method TEXT NOT NULL DEFAULT 'static'
    CHECK (forecast_method IN (
        'static',
        'previous_month',
        'average_previous_3',
        'same_month_last_year',
        'overall_average'
    ));
