-- Migration 003 — model accounts as cash-flow only: checking (spendable) + credit card
-- (liability). Fixes the rollup bug where a credit card's negative balance ADDED to
-- "what's left", and drops the `protected` flag whose role is now the account's type.
--
-- A credit card is entered as a limit + available credit; owed = limit − available, and
-- owed is subtracted from "what's left". Reserve/investment are out of scope (stock /
-- net-worth concepts), so any such rows are removed.

ALTER TABLE account ADD COLUMN credit_limit_cents     INTEGER;   -- credit cards only (NULL otherwise)
ALTER TABLE account ADD COLUMN available_credit_cents INTEGER;   -- credit cards only (NULL otherwise)

-- Existing cards carried a negative balance (money owed). Give them a placeholder limit
-- and zero the now-unused balance, preserving owed exactly:
--   owed      = limit − available
--   available = limit + old_balance          (old_balance <= 0)
-- The limit is at least $5,000 but never less than the owed amount, so `available` is
-- never negative (a card can't owe more than its limit). The user corrects the real limit
-- once; no real card data exists yet, so this effectively only touches the demo card.
UPDATE account
   SET credit_limit_cents = max(500000, -balance_cents),
       available_credit_cents = max(500000, -balance_cents) + balance_cents,
       balance_cents = 0
 WHERE type = 'credit_card';

-- Out of scope now — none expected in real data.
DELETE FROM account WHERE type IN ('reserve', 'investment');

-- Role is derived from `type`; the manual flag is redundant.
ALTER TABLE account DROP COLUMN protected;
