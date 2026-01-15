//! Integration tests for PgStore.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use ironflow::store::{
    BeginResult, DeadLetterQuery, EventStore, OutboxStore, PgStore, ProjectionStore, Store,
    UnitOfWork,
};
use ironflow::{
    InputObservation, Timer, Workflow, WorkflowId, WorkflowRuntime, WorkflowServiceConfig,
};
use serde::{Deserialize, Serialize};
use test_utils::db_test;

use crate::support::db::{
    count_events_for_workflow, count_timers, fetch_effect_id, fetch_outbox_payload,
    fetch_timer_input, insert_due_timer, insert_future_timer, insert_outbox_effect,
};
use crate::support::helpers::{
    DEFAULT_TEST_TIMEOUT, TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS, assert_event_types,
};
use crate::support::workflows::test_workflow::{
    TestWorkflow, TestWorkflowHandler, TestWorkflowInput,
};

/// Begin a workflow and expect it to be active (not completed).
///
/// Returns the events and unit of work for the workflow.
/// Returns an error if the workflow has already completed.
async fn begin_active<'a>(
    store: &'a PgStore,
    wf_type: &'static str,
    wf_id: &WorkflowId,
) -> anyhow::Result<(Vec<serde_json::Value>, <PgStore as Store>::UnitOfWork<'a>)> {
    match store.begin(wf_type, wf_id).await? {
        BeginResult::Active { events, uow, .. } => Ok((events, uow)),
        BeginResult::Completed => Err(anyhow::anyhow!(
            "expected Active for {wf_type}/{wf_id}, got Completed"
        )),
    }
}

// =============================================================================
// Test types for type-safe store testing
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum TestEvent {
    Created { name: String },
    A,
    B { value: i32 },
    C,
    First,
    Second,
    Event { task: i32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum TestEffect {
    SendEmail { to: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum TestTimerInput {
    Timeout { order_id: String },
}

fn build_test_timer(
    order_id: &str,
    fire_at: time::OffsetDateTime,
    key: Option<&str>,
) -> Timer<serde_json::Value> {
    let input = serde_json::to_value(TestTimerInput::Timeout {
        order_id: order_id.into(),
    })
    .expect("TestTimerInput should serialize");
    let timer = Timer::at(fire_at, input);
    match key {
        Some(k) => timer.with_key(k),
        None => timer,
    }
}

db_test!(begin_acquires_lock_and_returns_empty_events, |pool| {
    let store = PgStore::new(pool.clone());

    let (events, _uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;

    assert!(events.is_empty());
    Ok(())
});

db_test!(append_events_and_commit_persists, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.append_events([TestEvent::Created {
        name: "test".into(),
    }])
    .await?;
    uow.commit().await?;

    let count = count_events_for_workflow(pool, "test", "id-1").await?;
    assert_eq!(count, 1);
    Ok(())
});

db_test!(begin_returns_previously_committed_events, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.append_events([TestEvent::A, TestEvent::B { value: 42 }])
        .await?;
    uow.commit().await?;

    let (events, _uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;

    assert_event_types(&events, &["A", "B"]);
    assert_eq!(events[1]["value"], 42);
    Ok(())
});

db_test!(uncommitted_changes_are_rolled_back, |pool| {
    let store = PgStore::new(pool.clone());

    {
        let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
        uow.append_events([TestEvent::Created {
            name: "test".into(),
        }])
        .await?;
        // uow dropped without commit
    }

    let count = count_events_for_workflow(pool, "test", "id-1").await?;
    assert_eq!(count, 0);
    Ok(())
});

db_test!(enqueue_effects_persists_to_outbox, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.enqueue_effects([TestEffect::SendEmail {
        to: "test@example.com".into(),
    }])
    .await?;
    uow.commit().await?;

    let payload = fetch_outbox_payload(pool, "id-1").await?;
    assert_eq!(payload["type"], "SendEmail");
    assert_eq!(payload["to"], "test@example.com");
    Ok(())
});

db_test!(sequence_increments_per_stream, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.append_events([TestEvent::A, TestEvent::B { value: 0 }, TestEvent::C])
        .await?;
    uow.commit().await?;

    let sequences: Vec<i64> = sqlx::query_scalar!(
        "SELECT sequence FROM ironflow.events WHERE workflow_id = $1 ORDER BY sequence",
        "id-1"
    )
    .fetch_all(pool)
    .await?;

    assert_eq!(sequences, vec![1, 2, 3]);
    Ok(())
});

db_test!(global_sequence_is_monotonic_across_streams, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("a")).await?;
    uow.append_events([TestEvent::A]).await?;
    uow.commit().await?;

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("b")).await?;
    uow.append_events([TestEvent::B { value: 1 }]).await?;
    uow.commit().await?;

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("a")).await?;
    uow.append_events([TestEvent::A]).await?;
    uow.commit().await?;

    let sequences: Vec<i64> =
        sqlx::query_scalar!("SELECT global_sequence FROM ironflow.events ORDER BY global_sequence")
            .fetch_all(pool)
            .await?;

    assert_eq!(sequences.len(), 3);
    assert!(sequences[0] < sequences[1]);
    assert!(sequences[1] < sequences[2]);
    Ok(())
});

db_test!(different_workflow_types_are_isolated, |pool| {
    let store = PgStore::new(pool.clone());

    // Same workflow_id, different types - should be isolated
    let (_, mut uow) = begin_active(&store, "type_a", &WorkflowId::new("id-1")).await?;
    uow.append_events([TestEvent::A]).await?;
    uow.commit().await?;

    let (_, mut uow) = begin_active(&store, "type_b", &WorkflowId::new("id-1")).await?;
    uow.append_events([TestEvent::B { value: 0 }]).await?;
    uow.commit().await?;

    let (events_a, _) = begin_active(&store, "type_a", &WorkflowId::new("id-1")).await?;
    let (events_b, _) = begin_active(&store, "type_b", &WorkflowId::new("id-1")).await?;

    assert_eq!(events_a.len(), 1);
    assert_eq!(events_a[0]["type"], "A");
    assert_eq!(events_b.len(), 1);
    assert_eq!(events_b[0]["type"], "B");
    Ok(())
});

db_test!(schedule_timers_persists_to_timers_table, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    let fire_at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    uow.schedule_timers([build_test_timer("123", fire_at, None)])
        .await?;
    uow.commit().await?;

    let row = sqlx::query!(
        "SELECT workflow_type, workflow_id, fire_at, input FROM ironflow.timers WHERE workflow_id = $1",
        "id-1"
    )
    .fetch_one(pool)
    .await?;

    assert_eq!(row.workflow_type, "test");
    assert_eq!(row.workflow_id, "id-1");
    assert!(row.fire_at >= fire_at - time::Duration::seconds(1)); // timing tolerance
    assert_eq!(row.input["type"], "Timeout");
    assert_eq!(row.input["order_id"], "123");
    Ok(())
});

db_test!(schedule_timer_with_key_replaces_existing, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    let fire_at_1 = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    uow.schedule_timers([build_test_timer("v1", fire_at_1, Some("payment-timeout"))])
        .await?;
    uow.commit().await?;

    // Same key should replace, not add
    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    let fire_at_2 = time::OffsetDateTime::now_utc() + time::Duration::hours(2);
    uow.schedule_timers([build_test_timer("v2", fire_at_2, Some("payment-timeout"))])
        .await?;
    uow.commit().await?;

    let count = count_timers(pool, "test", "id-1", false, None).await?;
    assert_eq!(count, 1);

    let input = fetch_timer_input(pool, "test", "id-1").await?;
    assert_eq!(input["order_id"], "v2");
    Ok(())
});

db_test!(cancel_timers_by_key, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    let fire_at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    uow.schedule_timers([build_test_timer(
        "cancel-1",
        fire_at,
        Some("payment-timeout"),
    )])
    .await?;
    uow.commit().await?;

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.cancel_timers(vec!["payment-timeout".to_string()])
        .await?;
    uow.commit().await?;

    let count = count_timers(pool, "test", "id-1", true, None).await?;
    assert_eq!(count, 0);
    Ok(())
});

db_test!(record_input_observation_persists, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    let observation = InputObservation {
        workflow_type: "test".into(),
        workflow_id: WorkflowId::new("id-1"),
        input_type: "TestInput".into(),
        payload: serde_json::json!({ "type": "TestInput", "value": 42 }),
    };
    uow.record_input_observation(observation).await?;
    uow.commit().await?;

    let row = sqlx::query!(
        "SELECT input_type, payload FROM ironflow.input_observations WHERE workflow_id = $1",
        "id-1"
    )
    .fetch_one(pool)
    .await?;

    assert_eq!(row.input_type, "TestInput");
    assert_eq!(row.payload["value"], 42);
    Ok(())
});

db_test!(effect_permanent_failure_dead_letters_immediately, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("id-1")).await?;
    uow.enqueue_effects([TestEffect::SendEmail {
        to: "fail@example.com".into(),
    }])
    .await?;
    uow.commit().await?;

    let effect_id = fetch_effect_id(pool, "id-1").await?;

    store
        .record_permanent_failure(effect_id, "boom", TEST_MAX_ATTEMPTS)
        .await?;

    let dead_letters = store
        .fetch_dead_letters(&DeadLetterQuery::new(), TEST_MAX_ATTEMPTS)
        .await?;
    assert_eq!(dead_letters.len(), 1);

    let claimed = store
        .claim_effect("worker-1", DEFAULT_TEST_TIMEOUT, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(claimed.is_none());
    Ok(())
});

db_test!(timer_dead_letter_queries_and_retry, |pool| {
    let store = PgStore::new(pool.clone());

    let timer_id: uuid::Uuid = sqlx::query_scalar!(
        r#"
        INSERT INTO ironflow.timers (workflow_type, workflow_id, fire_at, input, attempts)
        VALUES ($1, $2, now(), $3, $4)
        RETURNING id
        "#,
        "test",
        "id-1",
        serde_json::json!({"type": "Timeout", "order_id": "dlq-1"}),
        TEST_MAX_ATTEMPTS as i32,
    )
    .fetch_one(pool)
    .await?;

    let dead_letters = store
        .fetch_timer_dead_letters(&DeadLetterQuery::new(), TEST_MAX_ATTEMPTS)
        .await?;
    assert_eq!(dead_letters.len(), 1);

    let count = store
        .count_timer_dead_letters(&DeadLetterQuery::new(), TEST_MAX_ATTEMPTS)
        .await?;
    assert_eq!(count, 1);

    let retried = store.retry_timer_dead_letter(timer_id).await?;
    assert!(retried);

    let row = sqlx::query!(
        "SELECT attempts FROM ironflow.timers WHERE id = $1",
        timer_id
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(row.attempts, 0);
    Ok(())
});

// =============================================================================
// Concurrent execution tests
// =============================================================================

db_test!(
    advisory_lock_blocks_concurrent_access_to_same_stream,
    |pool| {
        let store = PgStore::new(pool.clone());
        let workflow_id = WorkflowId::new("concurrent-1");

        // Coordination channels for deterministic ordering
        let (lock_acquired_tx, lock_acquired_rx) = tokio::sync::oneshot::channel::<()>();
        let (can_commit_tx, can_commit_rx) = tokio::sync::oneshot::channel::<()>();

        let store1 = store.clone();
        let wf_id1 = workflow_id.clone();
        let task1 = tokio::spawn(async move {
            let BeginResult::Active { mut uow, .. } = store1
                .begin("test", &wf_id1)
                .await
                .expect("begin should succeed")
            else {
                anyhow::bail!("expected Active, got Completed");
            };
            lock_acquired_tx
                .send(())
                .expect("receiver should not be dropped");
            uow.append_events([TestEvent::First])
                .await
                .expect("append should succeed");
            can_commit_rx.await.expect("sender should not be dropped");
            uow.commit().await.expect("commit should succeed");
            Ok::<_, anyhow::Error>(())
        });

        lock_acquired_rx
            .await
            .expect("task1 should signal lock acquired");

        // task2 will block on begin() until task1 commits
        let store2 = store.clone();
        let wf_id2 = workflow_id.clone();
        let task2 = tokio::spawn(async move {
            let BeginResult::Active {
                events, mut uow, ..
            } = store2
                .begin("test", &wf_id2)
                .await
                .expect("begin should succeed")
            else {
                anyhow::bail!("expected Active, got Completed");
            };
            uow.append_events([TestEvent::Second])
                .await
                .expect("append should succeed");
            uow.commit().await.expect("commit should succeed");
            Ok::<_, anyhow::Error>(events.len())
        });

        can_commit_tx
            .send(())
            .expect("task1 should still be waiting");

        task1.await.expect("task1 should complete")?;
        let events_seen_by_task2 = task2.await.expect("task2 should complete")?;

        // task2 seeing task1's event proves serialization
        assert_eq!(events_seen_by_task2, 1);

        let events: Vec<String> = sqlx::query_scalar!(
            "SELECT payload->>'type' FROM ironflow.events WHERE workflow_id = $1 ORDER BY sequence",
            "concurrent-1"
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .flatten()
        .collect();

        assert_eq!(events, vec!["First", "Second"]);
        Ok(())
    }
);

db_test!(
    different_workflow_instances_can_execute_concurrently,
    |pool| {
        let store = PgStore::new(pool.clone());
        let execution_order = Arc::new(AtomicI32::new(0));

        let mut handles = vec![];
        for i in 0..3 {
            let store = store.clone();
            let order = execution_order.clone();
            let handle = tokio::spawn(async move {
                let workflow_id = WorkflowId::new(format!("parallel-{i}"));
                let BeginResult::Active { mut uow, .. } = store
                    .begin("test", &workflow_id)
                    .await
                    .expect("begin should succeed")
                else {
                    anyhow::bail!("expected Active, got Completed");
                };

                let acquired = order.fetch_add(1, Ordering::SeqCst);

                uow.append_events([TestEvent::Event { task: i }])
                    .await
                    .expect("append should succeed");
                uow.commit().await.expect("commit should succeed");

                Ok::<_, anyhow::Error>(acquired)
            });
            handles.push(handle);
        }

        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.expect("task should complete")?);
        }

        results.sort();
        assert_eq!(results, vec![0, 1, 2]);

        let count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM ironflow.events WHERE workflow_type = $1 AND workflow_id LIKE 'parallel-%'",
            "test"
        )
        .fetch_one(pool)
        .await?
        .unwrap_or(0);

        assert_eq!(count, 3);
        Ok(())
    }
);

// =============================================================================
// Terminal state tests
// =============================================================================

db_test!(begin_returns_completed_for_completed_workflow, |pool| {
    let store = PgStore::new(pool.clone());
    let service = WorkflowRuntime::builder(store.clone(), WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .build_service()?;

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::stop("completed-begin-test"))
        .await?;

    let result = store
        .begin(TestWorkflow::TYPE, &WorkflowId::new("completed-begin-test"))
        .await?;
    assert!(matches!(result, BeginResult::Completed));

    Ok(())
});

// =============================================================================
// Effect processing tests (OutboxStore)
// =============================================================================

db_test!(claim_effect_returns_pending_effect, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("claim-1")).await?;
    uow.enqueue_effects([TestEffect::SendEmail {
        to: "claim@example.com".into(),
    }])
    .await?;
    uow.commit().await?;

    let effect = store
        .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;

    assert!(effect.is_some());
    let effect = effect.unwrap();
    assert_eq!(effect.workflow.workflow_id().as_str(), "claim-1");
    assert_eq!(effect.payload["type"], "SendEmail");
    assert_eq!(effect.attempts, 0);

    Ok(())
});

db_test!(claim_effect_returns_none_when_empty, |pool| {
    let store = PgStore::new(pool.clone());

    let effect = store
        .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;

    assert!(effect.is_none());
    Ok(())
});

db_test!(claim_effect_respects_lock_duration, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("lock-test")).await?;
    uow.enqueue_effects([TestEffect::SendEmail {
        to: "lock@example.com".into(),
    }])
    .await?;
    uow.commit().await?;

    let effect1 = store
        .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(effect1.is_some());

    // Second worker cannot claim - already locked
    let effect2 = store
        .claim_effect("worker-2", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(effect2.is_none());

    Ok(())
});

db_test!(mark_processed_removes_effect_from_queue, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("processed-1")).await?;
    uow.enqueue_effects([TestEffect::SendEmail {
        to: "done@example.com".into(),
    }])
    .await?;
    uow.commit().await?;

    let effect = store
        .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?
        .unwrap();

    store.mark_processed(effect.id).await?;

    let next = store
        .claim_effect("worker-2", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(next.is_none());

    let processed_at: Option<time::OffsetDateTime> = sqlx::query_scalar!(
        "SELECT processed_at FROM ironflow.outbox WHERE id = $1",
        effect.id
    )
    .fetch_one(pool)
    .await?;
    assert!(processed_at.is_some());

    Ok(())
});

db_test!(
    record_failure_increments_attempts_and_schedules_retry,
    |pool| {
        let store = PgStore::new(pool.clone());

        let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("retry-1")).await?;
        uow.enqueue_effects([TestEffect::SendEmail {
            to: "retry@example.com".into(),
        }])
        .await?;
        uow.commit().await?;

        let effect = store
            .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
            .await?
            .unwrap();

        store
            .record_failure(effect.id, "connection timeout", Duration::from_secs(1))
            .await?;

        let row = sqlx::query!(
            "SELECT attempts, last_error FROM ironflow.outbox WHERE id = $1",
            effect.id
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(row.attempts, 1);
        assert_eq!(row.last_error.as_deref(), Some("connection timeout"));

        // Not claimable yet due to backoff
        let next = store
            .claim_effect("worker-2", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
            .await?;
        assert!(next.is_none());

        Ok(())
    }
);

db_test!(retry_dead_letter_resets_attempts, |pool| {
    let store = PgStore::new(pool.clone());

    let effect_id = insert_outbox_effect(
        pool,
        "test",
        "dlq-retry-1",
        serde_json::json!({"type": "SendEmail", "to": "dlq@example.com"}),
        TEST_MAX_ATTEMPTS as i32,
        None,
    )
    .await?;

    let dead_letters = store
        .fetch_dead_letters(&DeadLetterQuery::new(), TEST_MAX_ATTEMPTS)
        .await?;
    assert_eq!(dead_letters.len(), 1);

    let retried = store.retry_dead_letter(effect_id).await?;
    assert!(retried);

    let attempts: i32 = sqlx::query_scalar!(
        "SELECT attempts FROM ironflow.outbox WHERE id = $1",
        effect_id
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(attempts, 0);

    let effect = store
        .claim_effect("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(effect.is_some());

    Ok(())
});

db_test!(count_dead_letters_returns_correct_count, |pool| {
    let store = PgStore::new(pool.clone());

    for i in 0..3 {
        insert_outbox_effect(
            pool,
            "test",
            &format!("count-dlq-{i}"),
            serde_json::json!({"type": "SendEmail", "to": format!("dlq{i}@example.com")}),
            TEST_MAX_ATTEMPTS as i32,
            None,
        )
        .await?;
    }

    let count = store
        .count_dead_letters(&DeadLetterQuery::new(), TEST_MAX_ATTEMPTS)
        .await?;
    assert_eq!(count, 3);

    let count_filtered = store
        .count_dead_letters(
            &DeadLetterQuery::new().workflow_type("other"),
            TEST_MAX_ATTEMPTS,
        )
        .await?;
    assert_eq!(count_filtered, 0);

    Ok(())
});

// =============================================================================
// Timer processing tests (OutboxStore)
// =============================================================================

db_test!(claim_timer_returns_due_timer, |pool| {
    let store = PgStore::new(pool.clone());

    insert_due_timer(
        pool,
        "test",
        "timer-claim-1",
        serde_json::json!({"type": "Timeout", "order_id": "123"}),
    )
    .await?;

    let timer = store
        .claim_timer("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;

    assert!(timer.is_some());
    let timer = timer.unwrap();
    assert_eq!(timer.workflow.workflow_id().as_str(), "timer-claim-1");
    assert_eq!(timer.payload["type"], "Timeout");

    Ok(())
});

db_test!(claim_timer_returns_none_for_future_timer, |pool| {
    let store = PgStore::new(pool.clone());

    insert_future_timer(
        pool,
        "test",
        "timer-future-1",
        serde_json::json!({"type": "Timeout", "order_id": "456"}),
    )
    .await?;

    let timer = store
        .claim_timer("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;

    assert!(timer.is_none());
    Ok(())
});

db_test!(mark_timer_processed_removes_from_queue, |pool| {
    let store = PgStore::new(pool.clone());

    insert_due_timer(
        pool,
        "test",
        "timer-processed-1",
        serde_json::json!({"type": "Timeout", "order_id": "789"}),
    )
    .await?;

    let timer = store
        .claim_timer("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?
        .unwrap();

    store.mark_timer_processed(timer.id).await?;

    let next = store
        .claim_timer("worker-2", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?;
    assert!(next.is_none());

    let processed_at: Option<time::OffsetDateTime> = sqlx::query_scalar!(
        "SELECT processed_at FROM ironflow.timers WHERE id = $1",
        timer.id
    )
    .fetch_one(pool)
    .await?;
    assert!(processed_at.is_some());

    Ok(())
});

db_test!(record_timer_failure_increments_attempts, |pool| {
    let store = PgStore::new(pool.clone());

    insert_due_timer(
        pool,
        "test",
        "timer-fail-1",
        serde_json::json!({"type": "Timeout", "order_id": "fail"}),
    )
    .await?;

    let timer = store
        .claim_timer("worker-1", TEST_LOCK_DURATION, TEST_MAX_ATTEMPTS)
        .await?
        .unwrap();

    store
        .record_timer_failure(timer.id, "workflow locked", Duration::from_secs(1))
        .await?;

    let row = sqlx::query!(
        "SELECT attempts, last_error FROM ironflow.timers WHERE id = $1",
        timer.id
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_error.as_deref(), Some("workflow locked"));

    Ok(())
});

// =============================================================================
// Projection store tests (EventStore + ProjectionStore)
// =============================================================================

db_test!(fetch_events_since_returns_events_after_sequence, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("proj-1")).await?;
    uow.append_events([TestEvent::A, TestEvent::B { value: 1 }])
        .await?;
    uow.commit().await?;

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("proj-2")).await?;
    uow.append_events([TestEvent::C]).await?;
    uow.commit().await?;

    let events = store.fetch_events_since(0, 100).await?;
    assert_eq!(events.len(), 3);

    let first_seq = events[0].global_sequence;
    let events_after = store.fetch_events_since(first_seq, 100).await?;
    assert_eq!(events_after.len(), 2);

    assert!(events_after[0].global_sequence < events_after[1].global_sequence);

    Ok(())
});

db_test!(fetch_events_since_respects_limit, |pool| {
    let store = PgStore::new(pool.clone());

    let (_, mut uow) = begin_active(&store, "test", &WorkflowId::new("limit-1")).await?;
    uow.append_events([TestEvent::A, TestEvent::B { value: 1 }, TestEvent::C])
        .await?;
    uow.commit().await?;

    let events = store.fetch_events_since(0, 2).await?;
    assert_eq!(events.len(), 2);

    Ok(())
});

db_test!(load_projection_position_returns_zero_for_new, |pool| {
    let store = PgStore::new(pool.clone());

    let position = store.load_projection_position("new-projection").await?;
    assert_eq!(position, 0);

    Ok(())
});

db_test!(store_and_load_projection_position, |pool| {
    let store = PgStore::new(pool.clone());

    let initial = store.load_projection_position("my-projection").await?;
    assert_eq!(initial, 0);

    store.store_projection_position("my-projection", 42).await?;

    let position = store.load_projection_position("my-projection").await?;
    assert_eq!(position, 42);

    store
        .store_projection_position("my-projection", 100)
        .await?;

    let position = store.load_projection_position("my-projection").await?;
    assert_eq!(position, 100);

    Ok(())
});

// =============================================================================
// Error handling tests
// =============================================================================

db_test!(event_deserialization_failure_includes_context, |pool| {
    // Insert valid events followed by a corrupted one
    sqlx::query!(
        r#"
        INSERT INTO ironflow.events (workflow_type, workflow_id, sequence, payload)
        VALUES
            ($1, $2, 1, $3),
            ($1, $2, 2, $4)
        "#,
        TestWorkflow::TYPE,
        "corrupted-1",
        serde_json::json!({"type": "Pinged"}),
        // Corrupted event - missing required fields, wrong structure
        serde_json::json!({"type": "InvalidEventType", "garbage": true}),
    )
    .execute(pool)
    .await?;

    let store = PgStore::new(pool.clone());
    let service = WorkflowRuntime::builder(store.clone(), WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .build_service()?;

    // Attempting to execute should fail during event replay
    let result = service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("corrupted-1"))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_string = err.to_string();

    // Error should include context about which event failed
    assert!(
        err_string.contains("test_workflow") || err_string.contains("corrupted-1"),
        "Error should include workflow context: {err_string}"
    );

    Ok(())
});
