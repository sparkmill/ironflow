//! PostgreSQL store implementation.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::outbox::{DeadLetter, DeadLetterQuery, OutboxEffect, OutboxStore};
use super::{
    BeginResult, EventStore, InputObservation, ProjectionStore, Store, StoredEvent, UnitOfWork,
    WorkflowInstanceSummary, WorkflowQueryStore,
};
use crate::Timer;
use crate::error::Result;
use crate::workflow::{WorkflowId, WorkflowRef};

/// PostgreSQL-backed store for production use.
///
/// Uses row-level locking via `SELECT ... FOR UPDATE` on the `workflow_instances`
/// table for per-stream concurrency control. The lock is held for the duration
/// of the transaction and released on commit.
///
/// # Database Schema
///
/// Requires tables in the `ironflow` schema:
///
/// | Table                | Purpose                                              |
/// |----------------------|------------------------------------------------------|
/// | `workflow_instances` | Row-level locking and workflow instance registry     |
/// | `events`             | Append-only event store with `global_sequence`       |
/// | `outbox`             | Effect queue for immediate side effects              |
/// | `timers`             | Timer queue for scheduled workflow inputs            |
///
/// # Concurrency
///
/// Different workflow instances can execute concurrently (different rows).
/// Same workflow instance is serialized (row lock blocks).
///
/// # Example
///
/// ```ignore
/// use ironflow::{Decider, PgStore};
/// use sqlx::PgPool;
///
/// let pool = PgPool::connect("postgres://...").await?;
/// let store = PgStore::new(pool);
/// let decider: Decider<MyWorkflow, _> = Decider::new(store);
/// ```
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

#[derive(sqlx::FromRow)]
struct WorkflowInstanceRow {
    workflow_type: String,
    workflow_id: String,
    created_at: time::OffsetDateTime,
    event_count: i64,
    last_event_at: Option<time::OffsetDateTime>,
    completed_at: Option<time::OffsetDateTime>,
}

impl PgStore {
    /// Create a new PostgreSQL store from a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn load_events_for_workflow(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query!(
            r#"
            SELECT global_sequence, workflow_type, workflow_id, sequence, payload, created_at
            FROM ironflow.events
            WHERE workflow_type = $1 AND workflow_id = $2
            ORDER BY sequence ASC
            "#,
            workflow_type,
            workflow_id.as_str()
        )
        .fetch_all(&mut **tx)
        .await?;

        let events = rows
            .into_iter()
            .map(|row| StoredEvent {
                global_sequence: row.global_sequence,
                workflow_type: row.workflow_type,
                workflow_id: WorkflowId::from(row.workflow_id),
                sequence: row.sequence,
                payload: row.payload,
                created_at: row.created_at,
            })
            .collect();

        Ok(events)
    }
}

#[async_trait]
impl WorkflowQueryStore for PgStore {
    async fn list_workflows(
        &self,
        workflow_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<WorkflowInstanceSummary>> {
        let mut builder = sqlx::QueryBuilder::new(
            r#"
            SELECT workflow_type, workflow_id, created_at, event_count, last_event_at, completed_at
            FROM ironflow.workflow_instances
            "#,
        );

        if let Some(workflow_type) = workflow_type {
            builder.push(" WHERE workflow_type = ");
            builder.push_bind(workflow_type);
        }

        builder.push(" ORDER BY last_event_at DESC NULLS LAST, created_at DESC");
        builder.push(" LIMIT ");
        builder.push_bind(limit as i64);
        builder.push(" OFFSET ");
        builder.push_bind(offset as i64);

        let rows = builder
            .build_query_as::<WorkflowInstanceRow>()
            .fetch_all(&self.pool)
            .await?;

        let workflows = rows
            .into_iter()
            .map(|row| WorkflowInstanceSummary {
                workflow_type: row.workflow_type,
                workflow_id: WorkflowId::from(row.workflow_id),
                created_at: row.created_at,
                event_count: row.event_count,
                last_event_at: row.last_event_at,
                completed_at: row.completed_at,
            })
            .collect();

        Ok(workflows)
    }

    async fn fetch_workflow_events(
        &self,
        workflow_type: &str,
        workflow_id: &WorkflowId,
    ) -> Result<Vec<StoredEvent>> {
        let mut tx = self.pool.begin().await?;
        self.load_events_for_workflow(&mut tx, workflow_type, workflow_id)
            .await
    }
}

impl Store for PgStore {
    type UnitOfWork<'a> = PgUnitOfWork<'a>;

    async fn begin<'a>(
        &'a self,
        workflow_type: &'static str,
        workflow_id: &WorkflowId,
    ) -> Result<BeginResult<Self::UnitOfWork<'a>>> {
        let mut tx = self.pool.begin().await?;
        let workflow_id_str = workflow_id.as_str();

        // Ensure workflow instance row exists (idempotent)
        sqlx::query!(
            r#"INSERT INTO ironflow.workflow_instances (workflow_type, workflow_id)
               VALUES ($1, $2)
               ON CONFLICT DO NOTHING"#,
            workflow_type,
            workflow_id_str,
        )
        .execute(&mut *tx)
        .await?;

        // Acquire row-level lock and check completion status
        let row = sqlx::query!(
            r#"SELECT completed_at FROM ironflow.workflow_instances
               WHERE workflow_type = $1 AND workflow_id = $2
               FOR UPDATE"#,
            workflow_type,
            workflow_id_str,
        )
        .fetch_one(&mut *tx)
        .await?;

        // If already completed, rollback and return early
        if row.completed_at.is_some() {
            // Transaction is rolled back on drop, releasing the lock
            return Ok(BeginResult::Completed);
        }

        // Load existing events for this stream
        let events = self
            .load_events_for_workflow(&mut tx, workflow_type, workflow_id)
            .await?;

        let next_sequence = events.len() as i64 + 1;

        let uow = PgUnitOfWork {
            tx,
            workflow_type,
            workflow_id: workflow_id_str.to_owned(),
            next_sequence,
            events_appended: 0,
            is_completed: false,
        };

        let payloads = events.into_iter().map(|event| event.payload).collect();

        Ok(BeginResult::Active {
            events: payloads,
            uow,
        })
    }
}

/// PostgreSQL unit of work.
///
/// Wraps a transaction with row-level lock held until commit.
pub struct PgUnitOfWork<'a> {
    tx: Transaction<'a, Postgres>,
    workflow_type: &'static str,
    workflow_id: String,
    next_sequence: i64,
    events_appended: i64,
    is_completed: bool,
}

impl UnitOfWork for PgUnitOfWork<'_> {
    async fn append_events<E, I>(&mut self, events: I) -> Result<()>
    where
        E: Serialize + Send,
        I: IntoIterator<Item = E> + Send,
    {
        // Collect to avoid holding iterator across await
        let events: Vec<_> = events.into_iter().collect();
        for event in events {
            let payload = serde_json::to_value(&event)?;

            sqlx::query!(
                r#"INSERT INTO ironflow.events (workflow_type, workflow_id, sequence, payload)
                   VALUES ($1, $2, $3, $4)"#,
                self.workflow_type,
                &self.workflow_id,
                self.next_sequence,
                payload,
            )
            .execute(&mut *self.tx)
            .await?;

            self.next_sequence += 1;
            self.events_appended += 1;
        }
        Ok(())
    }

    async fn enqueue_effects<F, I>(&mut self, effects: I) -> Result<()>
    where
        F: Serialize + Send,
        I: IntoIterator<Item = F> + Send,
    {
        // Collect to avoid holding iterator across await
        let effects: Vec<_> = effects.into_iter().collect();
        for effect in effects {
            let payload = serde_json::to_value(&effect)?;

            sqlx::query!(
                r#"INSERT INTO ironflow.outbox (workflow_type, workflow_id, payload)
                   VALUES ($1, $2, $3)"#,
                self.workflow_type,
                &self.workflow_id,
                payload,
            )
            .execute(&mut *self.tx)
            .await?;
        }
        Ok(())
    }

    async fn schedule_timers<T>(&mut self, timers: T) -> Result<()>
    where
        T: IntoIterator<Item = Timer<Value>> + Send,
    {
        // Collect to avoid holding iterator across await
        let timers: Vec<_> = timers.into_iter().collect();
        for timer in timers {
            if let Some(key) = &timer.key {
                // Timer with key: upsert (replace existing timer with same key)
                sqlx::query!(
                    r#"INSERT INTO ironflow.timers (workflow_type, workflow_id, fire_at, input, key)
                       VALUES ($1, $2, $3, $4, $5)
                       ON CONFLICT (workflow_type, workflow_id, key)
                       WHERE key IS NOT NULL AND processed_at IS NULL
                       DO UPDATE SET fire_at = EXCLUDED.fire_at, input = EXCLUDED.input, created_at = now()"#,
                    self.workflow_type,
                    &self.workflow_id,
                    timer.fire_at,
                    &timer.input,
                    key,
                )
                .execute(&mut *self.tx)
                .await?;
            } else {
                // Timer without key: simple insert
                sqlx::query!(
                    r#"INSERT INTO ironflow.timers (workflow_type, workflow_id, fire_at, input)
                       VALUES ($1, $2, $3, $4)"#,
                    self.workflow_type,
                    &self.workflow_id,
                    timer.fire_at,
                    &timer.input,
                )
                .execute(&mut *self.tx)
                .await?;
            }
        }
        Ok(())
    }

    async fn cancel_timers(&mut self, keys: Vec<String>) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }

        sqlx::query!(
            r#"
            UPDATE ironflow.timers
            SET processed_at = now(),
                locked_until = NULL,
                locked_by = NULL
            WHERE workflow_type = $1
              AND workflow_id = $2
              AND processed_at IS NULL
              AND key = ANY($3)
            "#,
            self.workflow_type,
            &self.workflow_id,
            &keys,
        )
        .execute(&mut *self.tx)
        .await?;

        Ok(())
    }

    async fn record_input_observation(&mut self, observation: InputObservation) -> Result<()> {
        sqlx::query!(
            r#"INSERT INTO ironflow.input_observations
               (workflow_type, workflow_id, input_type, payload)
               VALUES ($1, $2, $3, $4)"#,
            observation.workflow_type,
            observation.workflow_id.as_str(),
            observation.input_type,
            observation.payload,
        )
        .execute(&mut *self.tx)
        .await?;

        Ok(())
    }

    fn mark_completed(&mut self) {
        self.is_completed = true;
    }

    async fn commit(mut self) -> Result<()> {
        // Update monitoring metrics and terminal state
        if self.is_completed {
            sqlx::query!(
                r#"UPDATE ironflow.workflow_instances
                   SET event_count = event_count + $3,
                       last_event_at = now(),
                       completed_at = now()
                   WHERE workflow_type = $1 AND workflow_id = $2"#,
                self.workflow_type,
                &self.workflow_id,
                self.events_appended,
            )
            .execute(&mut *self.tx)
            .await?;
        } else if self.events_appended > 0 {
            sqlx::query!(
                r#"UPDATE ironflow.workflow_instances
                   SET event_count = event_count + $3,
                       last_event_at = now()
                   WHERE workflow_type = $1 AND workflow_id = $2"#,
                self.workflow_type,
                &self.workflow_id,
                self.events_appended,
            )
            .execute(&mut *self.tx)
            .await?;
        }

        self.tx.commit().await?;
        Ok(())
    }
}

impl EventStore for PgStore {
    async fn fetch_events_since(&self, after: i64, limit: u32) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query!(
            r#"
            SELECT global_sequence, workflow_type, workflow_id, sequence, payload, created_at
            FROM ironflow.events
            WHERE global_sequence > $1
            ORDER BY global_sequence
            LIMIT $2
            "#,
            after,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| StoredEvent {
                global_sequence: row.global_sequence,
                workflow_type: row.workflow_type,
                workflow_id: WorkflowId::new(row.workflow_id),
                sequence: row.sequence,
                payload: row.payload,
                created_at: row.created_at,
            })
            .collect())
    }
}

impl ProjectionStore for PgStore {
    async fn load_projection_position(&self, projection_name: &str) -> Result<i64> {
        sqlx::query!(
            r#"
            INSERT INTO ironflow.projection_positions (projection_name)
            VALUES ($1)
            ON CONFLICT (projection_name) DO NOTHING
            "#,
            projection_name,
        )
        .execute(&self.pool)
        .await?;

        let row = sqlx::query!(
            r#"
            SELECT last_sequence
            FROM ironflow.projection_positions
            WHERE projection_name = $1
            "#,
            projection_name,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(row.last_sequence)
    }

    async fn store_projection_position(
        &self,
        projection_name: &str,
        global_sequence: i64,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE ironflow.projection_positions
            SET last_sequence = $2,
                updated_at = now()
            WHERE projection_name = $1
            "#,
            projection_name,
            global_sequence,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

impl OutboxStore for PgStore {
    async fn claim_effect(
        &self,
        worker_id: &str,
        lock_duration: Duration,
        max_attempts: u32,
    ) -> Result<Option<OutboxEffect>> {
        // Atomically claim an immediate effect using FOR UPDATE SKIP LOCKED.
        // This prevents multiple workers from claiming the same effect.
        // Excludes dead-lettered effects (attempts >= max_attempts).
        //
        // Lock timestamp is computed in DB to avoid clock skew between app and DB servers.
        let lock_duration_secs = lock_duration.as_secs_f64();
        let row = sqlx::query!(
            r#"
            UPDATE ironflow.outbox
            SET locked_until = now() + ($1 * interval '1 second'),
                locked_by = $2
            WHERE id = (
                SELECT id FROM ironflow.outbox
                WHERE processed_at IS NULL
                  AND attempts < $3
                  AND (locked_until IS NULL OR locked_until < now())
                ORDER BY created_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING
                id,
                workflow_type,
                workflow_id,
                payload,
                attempts,
                created_at
            "#,
            lock_duration_secs,
            worker_id,
            max_attempts as i32,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| OutboxEffect {
            id: r.id,
            workflow: WorkflowRef::new(r.workflow_type, r.workflow_id),
            payload: r.payload,
            attempts: r.attempts as u32,
            created_at: r.created_at,
        }))
    }

    async fn mark_processed(&self, effect_id: Uuid) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE ironflow.outbox
            SET processed_at = now(),
                locked_until = NULL,
                locked_by = NULL
            WHERE id = $1
            "#,
            effect_id,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn record_failure(
        &self,
        effect_id: Uuid,
        error: &str,
        backoff_duration: Duration,
    ) -> Result<()> {
        // Backoff computed in DB to avoid clock skew between app and DB servers.
        let backoff_secs = backoff_duration.as_secs_f64();
        sqlx::query!(
            r#"
            UPDATE ironflow.outbox
            SET attempts = attempts + 1,
                last_error = $2,
                locked_until = now() + ($3 * interval '1 second'),
                locked_by = NULL
            WHERE id = $1
            "#,
            effect_id,
            error,
            backoff_secs,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn record_permanent_failure(
        &self,
        effect_id: Uuid,
        error: &str,
        max_attempts: u32,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE ironflow.outbox
            SET attempts = $2,
                last_error = $3,
                locked_until = NULL,
                locked_by = NULL
            WHERE id = $1
            "#,
            effect_id,
            max_attempts as i32,
            error,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn claim_timer(
        &self,
        worker_id: &str,
        lock_duration: Duration,
        max_attempts: u32,
    ) -> Result<Option<OutboxEffect>> {
        // Atomically claim a due timer from the dedicated timers table.
        // Excludes dead-lettered timers (attempts >= max_attempts).
        //
        // Lock timestamp is computed in DB to avoid clock skew between app and DB servers.
        let lock_duration_secs = lock_duration.as_secs_f64();
        let row = sqlx::query!(
            r#"
            UPDATE ironflow.timers
            SET locked_until = now() + ($1 * interval '1 second'),
                locked_by = $2
            WHERE id = (
                SELECT id FROM ironflow.timers
                WHERE fire_at <= now()
                  AND processed_at IS NULL
                  AND attempts < $3
                  AND (locked_until IS NULL OR locked_until < now())
                ORDER BY fire_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, workflow_type, workflow_id, input, attempts, created_at
            "#,
            lock_duration_secs,
            worker_id,
            max_attempts as i32,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| OutboxEffect {
            id: r.id,
            workflow: WorkflowRef::new(r.workflow_type, r.workflow_id),
            payload: r.input,
            attempts: r.attempts as u32,
            created_at: r.created_at,
        }))
    }

    async fn fetch_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> Result<Vec<DeadLetter>> {
        let workflow_id_str = query.workflow_id.as_ref().map(|id| id.as_str().to_owned());
        let limit = query.limit.unwrap_or(100) as i64;

        let rows = sqlx::query!(
            r#"
            SELECT
                id,
                workflow_type,
                workflow_id,
                payload,
                attempts,
                last_error,
                created_at
            FROM ironflow.outbox
            WHERE processed_at IS NULL
              AND attempts >= $1
              AND ($2::text IS NULL OR workflow_type = $2)
              AND ($3::text IS NULL OR workflow_id = $3)
            ORDER BY created_at DESC
            LIMIT $4
            "#,
            max_attempts as i32,
            query.workflow_type.as_deref(),
            workflow_id_str.as_deref(),
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DeadLetter {
                id: r.id,
                workflow: WorkflowRef::new(r.workflow_type, r.workflow_id),
                payload: r.payload,
                attempts: r.attempts as u32,
                last_error: r.last_error,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn retry_dead_letter(&self, effect_id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE ironflow.outbox
            SET attempts = 0,
                locked_until = NULL,
                locked_by = NULL,
                last_error = NULL
            WHERE id = $1
              AND processed_at IS NULL
            "#,
            effect_id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn count_dead_letters(&self, query: &DeadLetterQuery, max_attempts: u32) -> Result<u64> {
        let workflow_id_str = query.workflow_id.as_ref().map(|id| id.as_str().to_owned());

        let row = sqlx::query!(
            r#"
            SELECT COUNT(*) as "count!"
            FROM ironflow.outbox
            WHERE processed_at IS NULL
              AND attempts >= $1
              AND ($2::text IS NULL OR workflow_type = $2)
              AND ($3::text IS NULL OR workflow_id = $3)
            "#,
            max_attempts as i32,
            query.workflow_type.as_deref(),
            workflow_id_str.as_deref(),
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(row.count as u64)
    }

    async fn mark_timer_processed(&self, timer_id: Uuid) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE ironflow.timers
            SET processed_at = now(),
                locked_until = NULL,
                locked_by = NULL
            WHERE id = $1
            "#,
            timer_id,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn record_timer_failure(
        &self,
        timer_id: Uuid,
        error: &str,
        backoff_duration: Duration,
    ) -> Result<()> {
        // Backoff computed in DB to avoid clock skew between app and DB servers.
        let backoff_secs = backoff_duration.as_secs_f64();
        sqlx::query!(
            r#"
            UPDATE ironflow.timers
            SET attempts = attempts + 1,
                last_error = $2,
                locked_until = now() + ($3 * interval '1 second'),
                locked_by = NULL
            WHERE id = $1
            "#,
            timer_id,
            error,
            backoff_secs,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn fetch_timer_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> Result<Vec<DeadLetter>> {
        let workflow_id_str = query.workflow_id.as_ref().map(|id| id.as_str().to_owned());
        let limit = query.limit.unwrap_or(100) as i64;

        let rows = sqlx::query!(
            r#"
            SELECT
                id,
                workflow_type,
                workflow_id,
                input as "payload!",
                attempts,
                last_error,
                created_at
            FROM ironflow.timers
            WHERE processed_at IS NULL
              AND attempts >= $1
              AND ($2::text IS NULL OR workflow_type = $2)
              AND ($3::text IS NULL OR workflow_id = $3)
            ORDER BY created_at DESC
            LIMIT $4
            "#,
            max_attempts as i32,
            query.workflow_type.as_deref(),
            workflow_id_str.as_deref(),
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DeadLetter {
                id: r.id,
                workflow: WorkflowRef::new(r.workflow_type, r.workflow_id),
                payload: r.payload,
                attempts: r.attempts as u32,
                last_error: r.last_error,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn retry_timer_dead_letter(&self, timer_id: Uuid) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE ironflow.timers
            SET attempts = 0,
                locked_until = NULL,
                locked_by = NULL,
                last_error = NULL
            WHERE id = $1
              AND processed_at IS NULL
            "#,
            timer_id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn count_timer_dead_letters(
        &self,
        query: &DeadLetterQuery,
        max_attempts: u32,
    ) -> Result<u64> {
        let workflow_id_str = query.workflow_id.as_ref().map(|id| id.as_str().to_owned());

        let row = sqlx::query!(
            r#"
            SELECT COUNT(*) as "count!"
            FROM ironflow.timers
            WHERE processed_at IS NULL
              AND attempts >= $1
              AND ($2::text IS NULL OR workflow_type = $2)
              AND ($3::text IS NULL OR workflow_id = $3)
            "#,
            max_attempts as i32,
            query.workflow_type.as_deref(),
            workflow_id_str.as_deref(),
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(row.count as u64)
    }
}
