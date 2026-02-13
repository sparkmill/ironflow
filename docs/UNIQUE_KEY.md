# Unique Key Constraint

This document describes a feature for enforcing at-most-one-active-workflow
per external business key (e.g. one active price change per `listing_id`).

---

## Problem

Ironflow's concurrency lock is per `(workflow_type, workflow_id)`. When callers
generate a fresh UUID for each workflow, there is no way to enforce "only one
active workflow per external key". Two concurrent calls get different
workflow_ids and never contend.

---

## Design

### Schema: partial unique index

```sql
ALTER TABLE ironflow.workflow_instances ADD COLUMN unique_key TEXT;

CREATE UNIQUE INDEX idx_workflow_instances_active_unique_key
    ON ironflow.workflow_instances(workflow_type, unique_key)
    WHERE completed_at IS NULL AND unique_key IS NOT NULL;
```

The partial index only covers rows where `completed_at IS NULL`. Once a
workflow completes, it drops out of the index and a new workflow with the same
`unique_key` can be inserted.

### Workflow trait: opt-in method

```rust
pub trait Workflow {
    // ... existing ...

    /// Optional unique key for at-most-one-active-workflow constraint.
    /// If two workflows of the same type try to start with the same unique key,
    /// and the first hasn't completed yet, the second will fail with
    /// UniqueKeyConflict.
    fn unique_key(_input: &Self::Input) -> Option<String> {
        None
    }
}
```

Default is `None` — no constraint, fully backward-compatible.

### Store trait

Add `unique_key` parameter to `Store::begin()`:

```rust
pub trait Store {
    fn begin<'a>(
        &'a self,
        workflow_type: &'static str,
        workflow_id: &WorkflowId,
        unique_key: Option<&str>,
    ) -> impl Future<Output = Result<BeginResult<Self::UnitOfWork<'a>>>> + Send;
}
```

### PgStore::begin

The current code:

```rust
// (1) Idempotent insert by PK
INSERT INTO ironflow.workflow_instances (workflow_type, workflow_id)
VALUES ($1, $2)
ON CONFLICT DO NOTHING;

// (2) Lock and check completion
SELECT completed_at FROM ironflow.workflow_instances
WHERE workflow_type = $1 AND workflow_id = $2
FOR UPDATE;
```

Step (1) changes to:

```rust
INSERT INTO ironflow.workflow_instances (workflow_type, workflow_id, unique_key)
VALUES ($1, $2, $3)
ON CONFLICT (workflow_type, workflow_id) DO NOTHING;
```

The `ON CONFLICT` clause **only covers the PK**. If the partial unique index
on `(workflow_type, unique_key)` is violated (another active workflow with the
same key exists), PostgreSQL raises a constraint violation — it is _not_
swallowed by `DO NOTHING`.

Catch that error and return a typed result:

```rust
let result = sqlx::query!(
    r#"INSERT INTO ironflow.workflow_instances (workflow_type, workflow_id, unique_key)
       VALUES ($1, $2, $3)
       ON CONFLICT (workflow_type, workflow_id) DO NOTHING"#,
    workflow_type, workflow_id_str, unique_key,
)
.execute(&mut *tx)
.await;

match result {
    Ok(_) => {} // inserted or PK conflict (idempotent re-execution)
    Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
        return Err(Error::UniqueKeyConflict {
            workflow_type: workflow_type.to_string(),
            unique_key: unique_key.unwrap_or_default().to_string(),
        });
    }
    Err(e) => return Err(e.into()),
}
```

### decider::execute

Extract the unique key from input and pass it to `store.begin()`:

```rust
let unique_key = W::unique_key(input);
let (event_payloads, mut uow) = match store
    .begin(W::TYPE, &workflow_id, unique_key.as_deref())
    .await?
{
    BeginResult::Active { events, uow, .. } => (events, uow),
    BeginResult::Completed => return Ok(()),
};
```

### Error variant

```rust
#[error("unique key conflict: workflow type '{workflow_type}' already has \
         an active instance with key '{unique_key}'")]
UniqueKeyConflict {
    workflow_type: String,
    unique_key: String,
},
```

---

## Files to change

| File                    | Change                                                        |
| ----------------------- | ------------------------------------------------------------- |
| `migrations/`           | New migration: add `unique_key` column + partial unique index |
| `src/workflow.rs`       | Add `fn unique_key()` with default `None` to `Workflow` trait |
| `src/store/mod.rs`      | Add `unique_key: Option<&str>` param to `Store::begin()`      |
| `src/store/postgres.rs` | Update INSERT to include `unique_key`, catch unique violation |
| `src/decider.rs`        | Call `W::unique_key(input)` and pass to `store.begin()`       |
| `src/error.rs`          | Add `UniqueKeyConflict` variant                               |

---

## Concurrency guarantee

Two concurrent calls with different workflow_ids but the same unique_key:

1. Both generate different UUID workflow_ids
2. Both try to INSERT into `workflow_instances` with `unique_key = "listing-42"`
3. One succeeds, the other hits the partial unique index → `UniqueKeyConflict`
4. No race window — enforced by a PostgreSQL constraint

Once the first workflow completes (`completed_at` is set), the partial index
drops it. A new workflow for the same unique_key can start.

---

## Usage example

A payment workflow that ensures only one active payment per order:

```rust
impl Workflow for PaymentWorkflow {
    type State = PaymentState;
    type Input = PaymentInput;
    type Event = PaymentEvent;
    type Effect = PaymentEffect;

    const TYPE: &'static str = "payment";

    fn unique_key(input: &PaymentInput) -> Option<String> {
        match input {
            // Only the creation input needs the constraint
            PaymentInput::Create { order_id, .. } => {
                Some(format!("order-{order_id}"))
            }
            // Subsequent inputs target an existing instance — no constraint needed
            _ => None,
        }
    }

    fn is_terminal(state: &PaymentState) -> bool {
        matches!(state.status, PaymentStatus::Settled | PaymentStatus::Failed)
    }

    // ... evolve, decide ...
}
```

---

## Requirement: `is_terminal` must be implemented

The unique key constraint relies on `completed_at` being set when a workflow
finishes. `completed_at` is only set when `is_terminal()` returns `true`.

If a workflow implements `unique_key()` but not `is_terminal()` (which
defaults to `false`), the workflow will never be marked as completed,
the unique key will stay in the partial index forever, and no new workflow
with the same key can ever start.

**Rule: any workflow that uses `unique_key()` must also implement
`is_terminal()`.**

This could be enforced at registration time by checking that the workflow
type has a non-default `is_terminal` implementation, or documented as a
contract that callers must uphold.

---

## Design rationale

- **Zero race window**: enforced by a PostgreSQL unique index, not
  application-level checks
- **Opt-in**: existing workflows are untouched (`unique_key` defaults to
  `None`, column is nullable)
- **No new tables**: one column + one index on the existing
  `workflow_instances` table
- **Clean conflict separation**: `ON CONFLICT (workflow_type, workflow_id)
DO NOTHING` handles PK idempotency; the partial unique index handles
  cross-workflow-id uniqueness — PostgreSQL distinguishes between them
  naturally
- **Completed workflows don't block**: `WHERE completed_at IS NULL` in the
  partial index means only active workflows participate in the constraint
