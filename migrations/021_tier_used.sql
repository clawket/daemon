-- TIER-042: tier_used and escalation_reason columns on tasks.
-- tier      : declared default tier (low|med|high) — from envelope/policy.
-- tier_used : tier the agent actually executed at — written by the agent on update.
-- escalation_reason : non-empty when tier_used differs from tier (TIER-043).
ALTER TABLE tasks ADD COLUMN tier_used TEXT;
ALTER TABLE tasks ADD COLUMN escalation_reason TEXT;
