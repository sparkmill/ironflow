-- Add unique_key column for at-most-one-active-workflow constraint.
--
-- The partial unique index ensures that only one active (non-completed) workflow
-- can exist per (workflow_type, unique_key) combination. Once a workflow completes
-- (completed_at IS set), it drops out of the index and a new workflow with the
-- same unique_key can start.
ALTER TABLE ironflow.workflow_instances
    ADD COLUMN unique_key text;

CREATE UNIQUE INDEX idx_workflow_instances_active_unique_key ON ironflow.workflow_instances(workflow_type, unique_key)
WHERE
    completed_at IS NULL AND unique_key IS NOT NULL;
