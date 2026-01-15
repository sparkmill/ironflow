use ironflow::store::{BeginResult, UnitOfWork};
use ironflow::{PgStore, Result, Store, WorkflowId};
use serde::Serialize;
use sqlx::PgPool;

// =============================================================================
// Test setup helpers
// =============================================================================

pub async fn seed_events<E: Serialize + Send>(
    store: &PgStore,
    workflow_type: &'static str,
    workflow_id: &WorkflowId,
    events: Vec<E>,
) -> Result<()> {
    let BeginResult::Active { mut uow, .. } = store.begin(workflow_type, workflow_id).await? else {
        return Ok(());
    };

    uow.append_events(events).await?;
    uow.commit().await?;
    Ok(())
}

// =============================================================================
// Event queries
// =============================================================================

pub async fn fetch_events(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let events: Vec<serde_json::Value> = sqlx::query_scalar!(
        r#"SELECT payload FROM ironflow.events
           WHERE workflow_type = $1 AND workflow_id = $2
           ORDER BY sequence"#,
        workflow_type,
        workflow_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(events)
}

pub async fn count_events_for_workflow(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
) -> Result<i64> {
    let count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ironflow.events WHERE workflow_type = $1 AND workflow_id = $2",
        workflow_type,
        workflow_id
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    Ok(count)
}

/// Fetch events matching a workflow_id pattern (e.g., "parallel-%").
pub async fn fetch_events_matching(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id_pattern: &str,
) -> Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query!(
        r#"SELECT workflow_id, payload FROM ironflow.events
           WHERE workflow_type = $1 AND workflow_id LIKE $2
           ORDER BY workflow_id, sequence"#,
        workflow_type,
        workflow_id_pattern,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.workflow_id, r.payload))
        .collect())
}

pub async fn count_completed_workflows(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id_pattern: &str,
    event_type: &str,
) -> Result<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(DISTINCT workflow_id)
        FROM ironflow.events
        WHERE workflow_type = $1
        AND workflow_id LIKE $2
        AND payload->>'type' = $3
        "#,
        workflow_type,
        workflow_id_pattern,
        event_type
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    Ok(count)
}

// =============================================================================
// Effect/Outbox queries
// =============================================================================

pub async fn fetch_effects(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let effects: Vec<serde_json::Value> = sqlx::query_scalar!(
        r#"SELECT payload FROM ironflow.outbox
           WHERE workflow_type = $1 AND workflow_id = $2
           ORDER BY created_at"#,
        workflow_type,
        workflow_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(effects)
}

/// Count effects in the outbox with flexible filtering.
///
/// - `workflow_type`: Filter by workflow type (required)
/// - `processed`: None = all, Some(true) = processed only, Some(false) = pending only
/// - `workflow_ids`: Optional filter by specific workflow IDs
pub async fn count_effects(
    pool: &PgPool,
    workflow_type: &str,
    processed: Option<bool>,
    workflow_ids: Option<&[&str]>,
) -> Result<i64> {
    let mut query = String::from("SELECT COUNT(*) FROM ironflow.outbox WHERE workflow_type = $1");

    if let Some(is_processed) = processed {
        if is_processed {
            query.push_str(" AND processed_at IS NOT NULL");
        } else {
            query.push_str(" AND processed_at IS NULL");
        }
    }

    if workflow_ids.is_some() {
        query.push_str(" AND workflow_id = ANY($2)");
    }

    let count: i64 = if let Some(ids) = workflow_ids {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        sqlx::query_scalar(&query)
            .bind(workflow_type)
            .bind(&ids)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar(&query)
            .bind(workflow_type)
            .fetch_one(pool)
            .await?
    };

    Ok(count)
}

pub async fn is_effect_processed(pool: &PgPool, workflow_id: &str) -> Result<bool> {
    let processed_at: Option<time::OffsetDateTime> = sqlx::query_scalar!(
        "SELECT processed_at FROM ironflow.outbox WHERE workflow_id = $1",
        workflow_id
    )
    .fetch_one(pool)
    .await?;

    Ok(processed_at.is_some())
}

pub async fn fetch_effect_id(pool: &PgPool, workflow_id: &str) -> Result<uuid::Uuid> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM ironflow.outbox WHERE workflow_id = $1",
        workflow_id
    )
    .fetch_one(pool)
    .await?;

    Ok(id)
}

// =============================================================================
// Input observation queries
// =============================================================================

pub async fn fetch_input_observations(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
) -> Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query!(
        r#"SELECT input_type, payload FROM ironflow.input_observations
           WHERE workflow_type = $1 AND workflow_id = $2
           ORDER BY observed_at"#,
        workflow_type,
        workflow_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| (row.input_type, row.payload))
        .collect())
}

/// Insert an effect directly into the outbox for testing.
///
/// Useful for testing retry scenarios or pre-seeding failed effects.
pub async fn insert_outbox_effect(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
    payload: serde_json::Value,
    attempts: i32,
    last_error: Option<&str>,
) -> Result<uuid::Uuid> {
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO ironflow.outbox (workflow_type, workflow_id, payload, attempts, last_error)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
        workflow_type,
        workflow_id,
        payload,
        attempts,
        last_error,
    )
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Fetch the outbox payload for a workflow.
pub async fn fetch_outbox_payload(pool: &PgPool, workflow_id: &str) -> Result<serde_json::Value> {
    let payload = sqlx::query_scalar!(
        "SELECT payload FROM ironflow.outbox WHERE workflow_id = $1",
        workflow_id
    )
    .fetch_one(pool)
    .await?;

    Ok(payload)
}

// =============================================================================
// Timer helpers
// =============================================================================

/// Count timers for a workflow.
/// `pending_only`: only count unprocessed timers. `key`: filter by timer key.
pub async fn count_timers(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
    pending_only: bool,
    key: Option<&str>,
) -> Result<i64> {
    let mut query = String::from(
        "SELECT COUNT(*) FROM ironflow.timers WHERE workflow_type = $1 AND workflow_id = $2",
    );

    if pending_only {
        query.push_str(" AND processed_at IS NULL");
    }

    if key.is_some() {
        query.push_str(" AND key = $3");
    }

    let count: i64 = if let Some(k) = key {
        sqlx::query_scalar(&query)
            .bind(workflow_type)
            .bind(workflow_id)
            .bind(k)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar(&query)
            .bind(workflow_type)
            .bind(workflow_id)
            .fetch_one(pool)
            .await?
    };

    Ok(count)
}

/// Fetch timer input for a workflow.
pub async fn fetch_timer_input(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
) -> Result<serde_json::Value> {
    let input = sqlx::query_scalar!(
        "SELECT input FROM ironflow.timers WHERE workflow_type = $1 AND workflow_id = $2",
        workflow_type,
        workflow_id
    )
    .fetch_one(pool)
    .await?;

    Ok(input)
}

/// Insert a timer that's already due (fire_at in the past).
pub async fn insert_due_timer(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
    input: serde_json::Value,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO ironflow.timers (workflow_type, workflow_id, fire_at, input)
        VALUES ($1, $2, now() - interval '1 minute', $3)
        "#,
        workflow_type,
        workflow_id,
        input,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a timer that's not yet due (fire_at in the future).
pub async fn insert_future_timer(
    pool: &PgPool,
    workflow_type: &str,
    workflow_id: &str,
    input: serde_json::Value,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO ironflow.timers (workflow_type, workflow_id, fire_at, input)
        VALUES ($1, $2, now() + interval '1 hour', $3)
        "#,
        workflow_type,
        workflow_id,
        input,
    )
    .execute(pool)
    .await?;
    Ok(())
}

// =============================================================================
// General helpers
// =============================================================================

pub async fn count_events(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar!("SELECT COUNT(*) FROM ironflow.events")
        .fetch_one(pool)
        .await?
        .unwrap_or(0);
    Ok(count)
}
