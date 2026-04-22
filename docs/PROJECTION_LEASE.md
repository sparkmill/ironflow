# Projection Lease (TODO)

Draft design for multi-instance HA of projection workers. Captured for a
future session — nothing implemented yet.

---

## Problem

Projections today are single-worker, user-driven, and uncoordinated:

- `ProjectionWorker::new(...).run(shutdown_rx).await` is entirely user
  wiring; the framework doesn't spawn projection workers.
- Running two workers against the same projection name causes every
  event to be processed by both. Handlers must be idempotent (documented
  contract at `docs/PROJECTIONS.md:351`) but we still waste CPU and do
  duplicate read-model writes.
- No mechanism for "worker A dies, worker B takes over." HA requires
  manual coordination (systemd restart, Kubernetes leader election,
  etc.).

Goal: one active worker per projection at a time, automatic takeover
when the holder dies, works across process boundaries, uses only the
existing Postgres dependency.

---

## Design sketch

### Schema

New migration:

```sql
ALTER TABLE ironflow.projection_positions
    ADD COLUMN leased_by text,
    ADD COLUMN lease_expires_at timestamptz;

CREATE INDEX projection_positions_lease_idx
    ON ironflow.projection_positions (lease_expires_at)
    WHERE leased_by IS NOT NULL;
```

Existing rows get `NULL` lease fields — treated as "unheld" by the
acquisition query. Backward-compatible.

### Store API

```rust
pub trait ProjectionStore: Send + Sync + Clone + 'static {
    async fn load_projection_position(&self, projection_name: &str) -> Result<i64>;

    /// Write the checkpoint only if `worker_id` still holds a valid
    /// lease on the projection. Returns `false` if the lease has been
    /// lost (expired or taken over). The caller uses that signal to
    /// abort the current batch and return to the acquire loop.
    async fn store_projection_position(
        &self,
        projection_name: &str,
        worker_id: &str,
        global_sequence: i64,
    ) -> Result<bool>;

    /// Acquire or renew a lease. Returns `true` iff `worker_id` now
    /// holds it. Called once at startup and every `heartbeat_interval`
    /// thereafter.
    async fn try_acquire_projection_lease(
        &self,
        projection_name: &str,
        worker_id: &str,
        lease_duration: Duration,
    ) -> Result<bool>;

    /// Clear the lease on graceful shutdown. No-op if another worker
    /// already holds it.
    async fn release_projection_lease(
        &self,
        projection_name: &str,
        worker_id: &str,
    ) -> Result<()>;
}
```

### Acquisition SQL (single UPSERT)

```sql
INSERT INTO ironflow.projection_positions
    (projection_name, leased_by, lease_expires_at)
VALUES ($1, $2, now() + ($3 * interval '1 second'))
ON CONFLICT (projection_name) DO UPDATE
SET leased_by = EXCLUDED.leased_by,
    lease_expires_at = EXCLUDED.lease_expires_at
WHERE projection_positions.leased_by IS NULL
   OR projection_positions.leased_by = EXCLUDED.leased_by   -- self-renewal
   OR projection_positions.lease_expires_at < now()          -- takeover after expiry
RETURNING leased_by = $2 AS owned
```

`lease_expires_at` is computed DB-side, sidestepping app/DB clock skew.

### Lease-gated checkpoint SQL

```sql
UPDATE ironflow.projection_positions
SET last_sequence = $2, updated_at = now()
WHERE projection_name = $1
  AND leased_by = $3
  AND lease_expires_at > now()
  AND last_sequence < $2
```

`rows_affected() > 0` means "we still own it, write succeeded."
`rows_affected() == 0` means "lease lost or stale sequence" — caller
aborts batch.

### Config additions

```rust
pub struct ProjectionConfig {
    // existing fields...

    /// How long the lease is valid before another worker can take over.
    /// Must be > heartbeat_interval. Default: 30s.
    pub lease_duration: Duration,

    /// How often to renew the lease. Should be 1/3 to 1/2 of
    /// lease_duration. Default: 10s.
    pub heartbeat_interval: Duration,

    /// How long to wait between acquisition attempts when the lease is
    /// held by someone else. Default: 5s.
    pub acquire_retry_interval: Duration,
}
```

Defaults give ~30s takeover on worker death, one DB UPSERT per 10s per
projection during steady state.

### Worker loop

```rust
'outer: loop {
    // Acquire loop
    while !self.store.try_acquire_projection_lease(name, worker_id, lease_duration).await? {
        tokio::select! {
            _ = sleep(acquire_retry_interval) => {},
            _ = shutdown.changed() => if *shutdown.borrow() { break 'outer; },
        }
    }

    // Process loop
    loop {
        if self.should_renew() {
            if !self.store.try_acquire_projection_lease(name, worker_id, lease_duration).await? {
                break; // Lost during renewal, back to acquire.
            }
        }

        let events = self.store.fetch_events_since(position, batch_size).await?;
        if events.is_empty() {
            // Idle; wait then retry. Renew during wait as needed.
            tokio::select! {
                _ = sleep(poll_interval) => {},
                _ = shutdown.changed() => if *shutdown.borrow() { break 'outer; },
            }
            continue;
        }

        for event in events {
            let next = event.global_sequence;
            self.projection.handle(event.into()).await?;

            let written = self.store
                .store_projection_position(name, worker_id, next)
                .await?;
            if !written {
                break; // Lost the lease mid-batch.
            }
            position = next;
        }
    }
}

self.store.release_projection_lease(name, worker_id).await?;
```

---

## What this does NOT fix

**Handler side effects are not atomically gated on lease ownership.**
The handler's writes run in its own transaction, not ours. If the lease
expires between `handler.handle(event).await` returning and our
checkpoint write, the handler's side effects have already landed.

Mitigations already in place:

- Handler contract requires idempotence.
- Duplicate window is bounded by `heartbeat_interval` (we detect lease
  loss within one renewal cycle).
- Monotonic checkpoint guard means the final stored sequence is always
  the max any worker reached.

Could be improved by checking the lease _before_ each `handler.handle`
call (extra DB round-trip per event), but that only saves wasted
idempotent work — no correctness gain. Defer unless profiling shows it
matters.

---

## Edge cases to verify in tests

- Acquire on a fresh projection (row doesn't exist yet): INSERT path.
- Re-acquire own lease (heartbeat): extends `lease_expires_at`.
- Another worker's acquire while held: returns `false`, row unchanged.
- Takeover after expiry: backdate `lease_expires_at`, second worker's
  acquire returns `true`.
- Release owned lease: clears fields.
- Release someone else's lease: no-op.
- Lease-gated write succeeds while owned.
- Lease-gated write returns `false` after takeover.
- End-to-end: two workers against same projection, one processes, other
  waits; kill the first, second takes over within
  `acquire_retry_interval + lease_duration`.

---

## Backward compatibility

- Migration adds nullable columns with defaults — existing
  `projection_positions` rows stay valid.
- Old code without lease enforcement continues to work against the new
  schema (it just ignores the lease columns).
- Mixed rollout window: some workers enforce the lease, some don't.
  Guarantee during window degrades to current behavior (duplicate
  processing, no takeover) but never worse than today.

---

## Scope

Roughly:

- Migration: 15 lines
- `ProjectionStore` methods (acquire, release, gated write): ~40 lines
- Worker loop changes: ~60 lines
- `ProjectionConfig` additions: ~15 lines
- Tests: ~70-100 lines

Maybe a half-day of work.

---

## Open questions

1. **Fail hard or retry silently on acquisition failure?** Current
   proposal: sleep and retry (normal for rolling deploys). Alternative:
   return an error if startup acquisition fails, requiring explicit
   opt-in to the wait behavior.

2. **Should the monotonic `last_sequence < $2` guard stay?** With the
   lease-gated write, it's redundant for correctness (one worker at a
   time, positions always advance). But it's cheap and catches bugs
   where a stale in-process retry tries to write an old sequence.
   Probably keep.

3. **How does this interact with the existing `store_projection_position`
   fix landed for finding #2?** The UPSERT-with-monotonic-guard shipped
   will need to be replaced by the lease-gated UPDATE. Behavior for
   single-worker users is equivalent.

4. **Are we confident the `RETURNING leased_by = $2 AS owned` idiom
   works in sqlx?** Should verify or fall back to a two-step (UPSERT +
   SELECT).

5. **Should `ProjectionWorker::new` take the lease config, or should it
   live on `ProjectionConfig`?** Currently proposing `ProjectionConfig`
   since that's where other timing knobs live.

---

## Next session

Pick up from here, verify the open questions, then implement:

1. Migration + schema.
2. Store trait changes + PgStore impl.
3. Worker loop refactor.
4. Tests for each lifecycle transition.
5. Update `docs/PROJECTIONS.md` with the new semantics.
