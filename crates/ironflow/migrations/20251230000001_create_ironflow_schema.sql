-- Ironflow: Event-sourced workflow engine schema
CREATE SCHEMA IF NOT EXISTS ironflow;

-- Workflow instances: tracks known workflows, used for row-level locking
--
-- This table serves multiple purposes:
-- 1. Row-level locking via SELECT ... FOR UPDATE (zero collision risk)
-- 2. Queryable index of all workflow instances
-- 3. Monitoring metrics (event count, last activity)
-- 4. Terminal state tracking (completed workflows)
-- 5. Snapshot storage (for faster replay)
CREATE TABLE ironflow.workflow_instances(
    workflow_type text NOT NULL,
    workflow_id text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Monitoring: updated on each commit
    event_count bigint NOT NULL DEFAULT 0,
    last_event_at timestamptz,
    -- Terminal state: set when workflow reaches terminal state
    completed_at timestamptz,
    -- Snapshots: for faster replay (skip replaying all events)
    snapshot_sequence bigint, -- event sequence at which snapshot was taken
    snapshot_data jsonb, -- serialized workflow state
    snapshot_at timestamptz, -- when snapshot was taken
    PRIMARY KEY (workflow_type, workflow_id)
);

-- Active workflows by type (for listing/monitoring)
CREATE INDEX workflow_instances_active_idx ON ironflow.workflow_instances(workflow_type, last_event_at DESC)
WHERE
    completed_at IS NULL;

-- Completed workflows (for cleanup jobs)
CREATE INDEX workflow_instances_completed_idx ON ironflow.workflow_instances(completed_at)
WHERE
    completed_at IS NOT NULL;

-- Workflows needing snapshots (many events, no recent snapshot)
CREATE INDEX workflow_instances_snapshot_candidates_idx ON ironflow.workflow_instances(workflow_type, event_count DESC)
WHERE
    completed_at IS NULL;

-- Event Store: append-only, source of truth
CREATE TABLE ironflow.events(
    -- Global ordering for projections (strictly increasing, may have gaps)
    -- UNIQUE constraint creates an index, used for projection catchup
    global_sequence bigserial NOT NULL UNIQUE,
    -- Stream identity
    workflow_type text NOT NULL,
    workflow_id text NOT NULL,
    sequence bigint
        NOT NULL,
        -- Event data
        payload jsonb NOT NULL,
        created_at timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (workflow_type, workflow_id, SEQUENCE)
);

-- Outbox: task queue for pending effects (immediate side effects)
CREATE TABLE ironflow.outbox(
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    -- Source workflow
    workflow_type text NOT NULL,
    workflow_id text NOT NULL,
    -- Effect data
    payload jsonb NOT NULL,
    -- Worker coordination: prevents double-processing
    locked_until timestamptz, -- claim expires (handles worker crashes)
    locked_by text, -- worker identifier (e.g. "worker-1", hostname, or UUID)
    -- Retry tracking
    attempts int NOT NULL DEFAULT 0,
    last_error text,
    -- Timestamps
    created_at timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz -- NULL = pending, set = done
);

-- Effect worker: find immediate effects ready for processing
-- Note: locked_until check must be done at query time (now() is not immutable)
CREATE INDEX outbox_immediate_idx ON ironflow.outbox(created_at)
WHERE
    processed_at IS NULL;

-- Dead letter query: find effects that exceeded max attempts
-- Keep predicate flexible by indexing attempts for pending entries.
CREATE INDEX outbox_dead_letter_idx ON ironflow.outbox(attempts, created_at)
WHERE
    processed_at IS NULL;

-- Cleanup: find old processed entries for pruning
CREATE INDEX outbox_processed_idx ON ironflow.outbox(processed_at)
WHERE
    processed_at IS NOT NULL;

-- Workflow lookup: find pending effects for a specific workflow instance
CREATE INDEX outbox_workflow_idx ON ironflow.outbox(workflow_type, workflow_id)
WHERE
    processed_at IS NULL;

-- Timers: scheduled workflow inputs delivered at a future time
--
-- Timers are conceptually different from effects:
-- - Effects: Side effects executed immediately by EffectHandler
-- - Timers: Scheduled inputs delivered to the workflow's decide function
--
-- Having a separate table provides:
-- - Clear separation of concerns
-- - Simpler queries (no NULL checks on due_at)
-- - Support for timer key-based deduplication/replacement
-- - Better traceability in observability tools
CREATE TABLE ironflow.timers(
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    -- Target workflow
    workflow_type text NOT NULL,
    workflow_id text NOT NULL,
    -- Timer data
    fire_at timestamptz NOT NULL,
    input jsonb NOT NULL,
    -- Optional key for deduplication: if set, a new timer with the same key
    -- for the same workflow instance replaces the existing one
    key text,
    -- Worker coordination
    locked_until timestamptz,
    locked_by text,
    -- Retry tracking (for input execution failures)
    attempts int NOT NULL DEFAULT 0,
    last_error text,
    -- Timestamps
    created_at timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz
);

-- Timer worker: find due timers ready for processing
CREATE INDEX timers_due_idx ON ironflow.timers(fire_at)
WHERE
    processed_at IS NULL;

-- Key-based lookup for timer replacement (upsert)
-- Unique constraint allows ON CONFLICT for deduplication
CREATE UNIQUE INDEX timers_key_idx ON ironflow.timers(workflow_type, workflow_id, key)
WHERE
    key IS NOT NULL AND processed_at IS NULL;

-- Workflow lookup: find pending timers for a specific workflow instance
CREATE INDEX timers_workflow_idx ON ironflow.timers(workflow_type, workflow_id)
WHERE
    processed_at IS NULL;

-- Cleanup: find old processed timers for pruning
CREATE INDEX timers_processed_idx ON ironflow.timers(processed_at)
WHERE
    processed_at IS NOT NULL;

-- Ironflow: input observations and projection positions
CREATE TABLE IF NOT EXISTS ironflow.input_observations(
    id bigserial PRIMARY KEY,
    workflow_type text NOT NULL,
    workflow_id text NOT NULL,
    input_type text NOT NULL,
    payload jsonb NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS input_observations_workflow_idx ON ironflow.input_observations(workflow_type, workflow_id, observed_at DESC);

CREATE TABLE IF NOT EXISTS ironflow.projection_positions(
    projection_name text PRIMARY KEY,
    last_sequence bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now()
);
