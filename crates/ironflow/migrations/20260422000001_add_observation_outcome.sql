-- Extend input_observations to record what happened to each input.
--
-- Prior to this migration, only accepted inputs were observed (Active branch
-- of decider::execute). With Outcome::Reject and ExecuteOutcome::AlreadyCompleted,
-- observations now cover all three caller-visible outcomes.
--
-- Backfill: existing rows are by definition accepted inputs, so the default
-- 'accepted' is correct. rejection_payload stays NULL for those.
ALTER TABLE ironflow.input_observations
    ADD COLUMN outcome text NOT NULL DEFAULT 'accepted',
    ADD COLUMN rejection_payload jsonb;

-- Enforce the invariant: rejection_payload is present iff outcome = 'rejected'.
ALTER TABLE ironflow.input_observations
    ADD CONSTRAINT input_observations_rejection_payload_matches_outcome CHECK ((outcome = 'rejected' AND rejection_payload IS NOT NULL) OR (outcome <> 'rejected' AND rejection_payload IS NULL));

-- Index for querying rejections specifically (common audit use case).
CREATE INDEX input_observations_rejected_idx ON ironflow.input_observations(workflow_type, workflow_id, observed_at DESC)
WHERE
    outcome = 'rejected';
