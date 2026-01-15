//! Integration tests for the effect runtime with PgStore.

use std::sync::Arc;
use std::time::Duration;

use crate::db_test;
use async_trait::async_trait;
use ironflow::{
    DeadLetterQuery, EffectContext, EffectHandler, OutboxStore, PgStore, RuntimeConfig, Timer,
    Workflow, WorkflowId,
};

use crate::support::db;
use crate::support::helpers::{
    ConcurrencyTracker, DEFAULT_POLL_INTERVAL, DEFAULT_TEST_TIMEOUT, TEST_MAX_ATTEMPTS, TestApp,
    assert_event_types, init_test_tracing, test_runtime_config, wait_until,
};
use crate::support::workflows::test_workflow::{
    EffectMode, TestWorkflow, TestWorkflowEffect, TestWorkflowHandler, TestWorkflowInput,
};
use crate::support::workflows::timer_test::{TimerTestHandler, TimerTestInput, TimerTestWorkflow};

/// TestWorkflowHandler wrapper that tracks concurrent executions.
struct TrackedTestHandler {
    delay: Duration,
    tracker: Arc<ConcurrencyTracker>,
    inner: TestWorkflowHandler,
}

#[async_trait]
impl EffectHandler for TrackedTestHandler {
    type Workflow = TestWorkflow;
    type Error = anyhow::Error;

    async fn handle(
        &self,
        effect: &TestWorkflowEffect,
        ctx: &EffectContext,
    ) -> Result<Option<TestWorkflowInput>, Self::Error> {
        self.tracker.enter();
        tokio::time::sleep(self.delay).await;
        let result = self.inner.handle(effect, ctx).await;
        self.tracker.exit();
        result
    }
}

// =============================================================================
// Integration Tests
// =============================================================================

db_test!(shutdown_completes_promptly, |pool| {
    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "shutdown-test",
            EffectMode::RouteResult,
        ))
        .await?;

    let shutdown_timeout = Duration::from_secs(2);
    tokio::time::timeout(shutdown_timeout, app.shutdown())
        .await
        .expect("shutdown should complete within timeout")?;

    Ok(())
});

db_test!(full_effect_cycle, |pool| {
    init_test_tracing();

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "api-test-1",
            EffectMode::RouteResult,
        ))
        .await?;

    // Wait for effect to be processed and result routed back
    let events = app
        .wait_for_events(TestWorkflow::TYPE, "api-test-1", 2, DEFAULT_TEST_TIMEOUT)
        .await?;

    assert_event_types(&events, &["Started", "Completed"]);

    // Verify effect payload was correctly enqueued with expected structure
    let effects = db::fetch_effects(pool, TestWorkflow::TYPE, "api-test-1").await?;
    assert_eq!(effects.len(), 1);
    let effect = &effects[0];
    assert_eq!(effect["type"], "Process");
    assert_eq!(effect["mode"]["mode"], "RouteResult");

    // Verify effect was processed (not pending)
    let processed_effects = db::count_effects(pool, TestWorkflow::TYPE, Some(true), None).await?;
    assert_eq!(processed_effects, 1);

    Ok(())
});

db_test!(effect_with_previous_failure_gets_processed, |pool| {
    // Pre-insert a failed effect before starting the runtime
    db::insert_outbox_effect(
        pool,
        TestWorkflow::TYPE,
        "retry-test-1",
        serde_json::json!({
            "type": "Process",
            "mode": { "mode": "RouteResult" }
        }),
        1,
        Some("Connection timeout"),
    )
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    app.wait_for_effect_processed("retry-test-1", DEFAULT_TEST_TIMEOUT)
        .await?;

    Ok(())
});

db_test!(multiple_workflows_same_type_different_ids, |pool| {
    init_test_tracing();

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "multi-1",
            EffectMode::RouteResult,
        ))
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "multi-2",
            EffectMode::RouteResult,
        ))
        .await?;

    let pool_ref = pool.clone();
    wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let events_1 =
            db::count_events_for_workflow(&pool_ref, TestWorkflow::TYPE, "multi-1").await?;
        let events_2 =
            db::count_events_for_workflow(&pool_ref, TestWorkflow::TYPE, "multi-2").await?;

        if events_1 >= 2 && events_2 >= 2 {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    })
    .await?;

    let events_1 = db::fetch_events(pool, TestWorkflow::TYPE, "multi-1").await?;
    let events_2 = db::fetch_events(pool, TestWorkflow::TYPE, "multi-2").await?;
    assert_event_types(&events_1, &["Started", "Completed"]);
    assert_event_types(&events_2, &["Started", "Completed"]);

    Ok(())
});

db_test!(multiple_workflow_types_in_same_runtime, |pool| {
    init_test_tracing();

    // Register BOTH workflow types in the same runtime
    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .register(TimerTestHandler)
        .build_and_run()
        .await?;

    // Execute TestWorkflow
    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "multi-type-test",
            EffectMode::RouteResult,
        ))
        .await?;

    // Execute TimerTestWorkflow (with short timeout that won't fire)
    app.service
        .execute::<TimerTestWorkflow>(&TimerTestInput::Start {
            id: "multi-type-timer".into(),
            timeout_secs: 3600, // Won't fire during test
            timer_key: Some("timeout".into()),
        })
        .await?;

    // Wait for both workflows to complete their effect processing
    let pool_ref = pool.clone();
    wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let test_events =
            db::count_events_for_workflow(&pool_ref, TestWorkflow::TYPE, "multi-type-test").await?;
        let timer_events =
            db::count_events_for_workflow(&pool_ref, TimerTestWorkflow::TYPE, "multi-type-timer")
                .await?;

        // TestWorkflow: Started + Completed = 2
        // TimerTestWorkflow: Started + Completed + TimerCancelled = 3
        if test_events >= 2 && timer_events >= 3 {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    })
    .await?;

    // Verify correct routing - each workflow type processed by its own handler
    let test_events = db::fetch_events(pool, TestWorkflow::TYPE, "multi-type-test").await?;
    let timer_events = db::fetch_events(pool, TimerTestWorkflow::TYPE, "multi-type-timer").await?;

    assert_event_types(&test_events, &["Started", "Completed"]);
    assert_event_types(&timer_events, &["Started", "Completed", "TimerCancelled"]);

    Ok(())
});

db_test!(parallel_effect_processing_with_multiple_workers, |pool| {
    init_test_tracing();

    let delay = Duration::from_millis(500);
    let tracker = ConcurrencyTracker::new();

    let app = TestApp::builder(pool)
        .register(TrackedTestHandler {
            delay,
            tracker: tracker.clone(),
            inner: TestWorkflowHandler::new(),
        })
        .config(RuntimeConfig {
            effect_poll_interval: Duration::from_millis(10),
            effect_workers: 3,
            ..test_runtime_config()
        })
        .build_and_run()
        .await?;

    // Submit 3 effects to maximize chance of observing parallelism
    for i in 1..=3 {
        app.service
            .execute::<TestWorkflow>(&TestWorkflowInput::start(
                format!("parallel-{i}"),
                EffectMode::RouteResult,
            ))
            .await?;
    }

    let pool_ref = pool.clone();
    wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let completed =
            db::count_completed_workflows(&pool_ref, TestWorkflow::TYPE, "parallel-%", "Completed")
                .await?;
        if completed >= 3 {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    })
    .await?;

    // With 3 workers and 500ms delays, we should observe at least 2 concurrent executions
    assert!(
        tracker.max_concurrent() >= 2,
        "Expected parallel execution with 3 workers, got max_concurrent={}",
        tracker.max_concurrent()
    );

    // Verify all workflows completed with expected events
    let events = db::fetch_events_matching(pool, TestWorkflow::TYPE, "parallel-%").await?;
    for i in 1..=3 {
        let wf_events: Vec<_> = events
            .iter()
            .filter(|(id, _)| id == &format!("parallel-{i}"))
            .collect();
        assert_eq!(
            wf_events.len(),
            2,
            "workflow parallel-{i} should have 2 events"
        );
        assert_eq!(wf_events[0].1["type"], "Started");
        assert_eq!(wf_events[1].1["type"], "Completed");
    }

    Ok(())
});

// =============================================================================
// Coverage gap tests
// =============================================================================

db_test!(unknown_workflow_type_dead_letters_immediately, |pool| {
    // This test documents the runtime's policy for unknown workflow types:
    // - Unknown type = permanent configuration error (not transient)
    // - Retrying won't help (no handler will ever exist for this type)
    // - Dead-letter immediately to avoid infinite retry loops
    //
    // If this policy needs to change (e.g., to support dynamic handler registration),
    // this test should be updated to reflect the new expected behavior.

    // Insert an effect for a workflow type that won't be registered
    db::insert_outbox_effect(
        pool,
        "nonexistent_workflow",
        "unknown-type-1",
        serde_json::json!({
            "type": "SomeEffect",
            "data": "test"
        }),
        0,
        None,
    )
    .await?;

    // Start runtime with only TestWorkflowHandler - doesn't handle "nonexistent_workflow"
    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Effect should be dead-lettered immediately (unknown type = permanent failure)
    app.wait_for_dead_letter(
        DeadLetterQuery::new().workflow_type("nonexistent_workflow"),
        DEFAULT_TEST_TIMEOUT,
    )
    .await?;

    Ok(())
});

db_test!(timer_worker_processes_due_timer_directly, |pool| {
    init_test_tracing();

    // Insert a due timer directly (bypasses workflow scheduling)
    db::insert_due_timer(
        pool,
        TestWorkflow::TYPE,
        "timer-direct-1",
        serde_json::json!({
            "type": "Start",
            "id": "timer-direct-1",
            "mode": { "mode": "RouteResult" }
        }),
    )
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Timer should fire and execute the workflow input
    let events = app
        .wait_for_events(
            TestWorkflow::TYPE,
            "timer-direct-1",
            2,
            DEFAULT_TEST_TIMEOUT,
        )
        .await?;

    assert_event_types(&events, &["Started", "Completed"]);

    Ok(())
});

db_test!(effect_transient_failure_retries_then_succeeds, |pool| {
    init_test_tracing();

    let handler = TestWorkflowHandler::new();

    let app = TestApp::builder(pool)
        .register(handler)
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "transient-1",
            EffectMode::TransientFailure,
        ))
        .await?;

    // Should fail first attempt, succeed on retry
    let events = app
        .wait_for_events(TestWorkflow::TYPE, "transient-1", 2, DEFAULT_TEST_TIMEOUT)
        .await?;

    assert_event_types(&events, &["Started", "Completed"]);

    Ok(())
});

db_test!(effect_permanent_failure_goes_to_dead_letter, |pool| {
    init_test_tracing();

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "permanent-1",
            EffectMode::PermanentFailure,
        ))
        .await?;

    // Should exhaust retries and end up in dead letter queue
    app.wait_for_dead_letter(
        DeadLetterQuery::new().workflow_id(WorkflowId::new("permanent-1")),
        DEFAULT_TEST_TIMEOUT,
    )
    .await?;

    // Workflow should only have the Started event (no Completed/Failed routed back)
    let events = db::fetch_events(pool, TestWorkflow::TYPE, "permanent-1").await?;
    assert_event_types(&events, &["Started"]);

    Ok(())
});

db_test!(effect_routing_failure_retries_effect, |pool| {
    init_test_tracing();

    // Pre-seed corrupted events for a workflow - this will cause execute_dynamic to fail
    // when the effect handler tries to route the result back
    sqlx::query!(
        r#"
        INSERT INTO ironflow.events (workflow_type, workflow_id, sequence, payload)
        VALUES ($1, $2, 1, $3)
        "#,
        TestWorkflow::TYPE,
        "routing-fail-target",
        // Corrupted event that will fail deserialization during replay
        serde_json::json!({"type": "InvalidEventType", "garbage": true}),
    )
    .execute(pool)
    .await?;

    // Insert an effect that will try to route to the corrupted workflow
    db::insert_outbox_effect(
        pool,
        TestWorkflow::TYPE,
        "routing-fail-target",
        serde_json::json!({
            "type": "Process",
            "mode": { "mode": "RouteResult" }
        }),
        0,
        None,
    )
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // The effect should fail routing and eventually dead-letter
    app.wait_for_dead_letter(
        DeadLetterQuery::new().workflow_id(WorkflowId::new("routing-fail-target")),
        DEFAULT_TEST_TIMEOUT,
    )
    .await?;

    Ok(())
});

db_test!(timer_execution_failure_retries_and_dead_letters, |pool| {
    init_test_tracing();

    // Pre-seed corrupted events that will cause timer execution to fail
    sqlx::query!(
        r#"
        INSERT INTO ironflow.events (workflow_type, workflow_id, sequence, payload)
        VALUES ($1, $2, 1, $3)
        "#,
        TestWorkflow::TYPE,
        "timer-fail-1",
        serde_json::json!({"type": "InvalidEventType"}),
    )
    .execute(pool)
    .await?;

    // Insert a due timer for the corrupted workflow
    db::insert_due_timer(
        pool,
        TestWorkflow::TYPE,
        "timer-fail-1",
        serde_json::json!({
            "type": "Ping",
            "id": "timer-fail-1"
        }),
    )
    .await?;

    let _app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Timer should fail and eventually dead-letter
    let store = PgStore::new(pool.clone());
    let dead_letters = wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let dls = store
            .fetch_timer_dead_letters(
                &DeadLetterQuery::new().workflow_id(WorkflowId::new("timer-fail-1")),
                TEST_MAX_ATTEMPTS,
            )
            .await
            .ok();
        match dls {
            Some(d) if !d.is_empty() => Ok(Some(d)),
            _ => Ok(None),
        }
    })
    .await?;

    assert!(!dead_letters.is_empty());

    Ok(())
});

db_test!(timer_with_corrupted_input_dead_letters, |pool| {
    init_test_tracing();

    // Insert a due timer with input that can't be deserialized to TestWorkflowInput.
    // This tests the timer worker's handling of malformed timer payloads.
    // The timer input JSON doesn't match any variant of TestWorkflowInput.
    db::insert_due_timer(
        pool,
        TestWorkflow::TYPE,
        "corrupt-timer-1",
        serde_json::json!({
            "type": "NonexistentInputType",
            "garbage": true
        }),
    )
    .await?;

    let _app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Timer should fail input deserialization and eventually dead-letter
    let store = PgStore::new(pool.clone());
    let dead_letters = wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let dls = store
            .fetch_timer_dead_letters(
                &DeadLetterQuery::new().workflow_id(WorkflowId::new("corrupt-timer-1")),
                TEST_MAX_ATTEMPTS,
            )
            .await
            .ok();
        match dls {
            Some(d) if !d.is_empty() => Ok(Some(d)),
            _ => Ok(None),
        }
    })
    .await?;

    assert!(!dead_letters.is_empty());
    // Verify error message indicates deserialization failure
    let dl = &dead_letters[0];
    assert!(
        dl.last_error
            .as_ref()
            .map(|e| e.contains("unknown variant") || e.contains("deserialize"))
            .unwrap_or(false),
        "Expected deserialization error, got: {:?}",
        dl.last_error
    );

    Ok(())
});

db_test!(timer_key_scoped_to_workflow_instance, |pool| {
    use ironflow::store::{BeginResult, Store, UnitOfWork};
    let store = PgStore::new(pool.clone());

    // Schedule timers with same key but different workflow instances
    let BeginResult::Active { mut uow, .. } = store
        .begin("test", &WorkflowId::new("timer-scope-a"))
        .await?
    else {
        anyhow::bail!("expected Active");
    };
    let fire_at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    uow.schedule_timers([
        Timer::at(fire_at, serde_json::json!({"type": "Timeout", "id": "a"}))
            .with_key("shared-key"),
    ])
    .await?;
    uow.commit().await?;

    let BeginResult::Active { mut uow, .. } = store
        .begin("test", &WorkflowId::new("timer-scope-b"))
        .await?
    else {
        anyhow::bail!("expected Active");
    };
    uow.schedule_timers([
        Timer::at(fire_at, serde_json::json!({"type": "Timeout", "id": "b"}))
            .with_key("shared-key"),
    ])
    .await?;
    uow.commit().await?;

    // Both timers should exist (key is scoped to workflow instance)
    let count_a =
        db::count_timers(pool, "test", "timer-scope-a", false, Some("shared-key")).await?;
    let count_b =
        db::count_timers(pool, "test", "timer-scope-b", false, Some("shared-key")).await?;

    assert_eq!(count_a, 1, "timer for workflow A should exist");
    assert_eq!(count_b, 1, "timer for workflow B should exist");

    Ok(())
});

db_test!(lock_expiration_allows_retry_by_another_worker, |pool| {
    // Simulate a workflow that was started and had its effect locked by a crashed worker.
    // First, pre-seed the Started event (workflow was already started).
    sqlx::query!(
        r#"
        INSERT INTO ironflow.events (workflow_type, workflow_id, sequence, payload)
        VALUES ($1, $2, 1, $3)
        "#,
        TestWorkflow::TYPE,
        "lock-expire-1",
        serde_json::json!({"type": "Started", "mode": "route_result"}),
    )
    .execute(pool)
    .await?;

    // Insert an effect with a lock that's already expired (simulating crashed worker).
    let _effect_id = sqlx::query_scalar!(
        r#"
        INSERT INTO ironflow.outbox (workflow_type, workflow_id, payload, locked_until, locked_by)
        VALUES ($1, $2, $3, now() - interval '1 minute', 'crashed-worker')
        RETURNING id
        "#,
        TestWorkflow::TYPE,
        "lock-expire-1",
        serde_json::json!({"type": "Process", "mode": { "mode": "RouteResult" }}),
    )
    .fetch_one(pool)
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Effect should be claimed by new worker and processed
    app.wait_for_effect_processed("lock-expire-1", DEFAULT_TEST_TIMEOUT)
        .await?;

    // Verify the effect was processed (not just claimed)
    let events = app
        .wait_for_events(TestWorkflow::TYPE, "lock-expire-1", 2, DEFAULT_TEST_TIMEOUT)
        .await?;

    assert_event_types(&events, &["Started", "Completed"]);

    Ok(())
});

db_test!(dead_letter_query_filters_by_both_type_and_id, |pool| {
    // Insert dead letters for different workflows
    for i in 0..3 {
        db::insert_outbox_effect(
            pool,
            "type_a",
            &format!("dlq-combo-a-{i}"),
            serde_json::json!({"type": "Effect"}),
            3, // max_attempts = 3 in test config
            Some("test error"),
        )
        .await?;
    }
    db::insert_outbox_effect(
        pool,
        "type_b",
        "dlq-combo-b-1",
        serde_json::json!({"type": "Effect"}),
        3,
        Some("test error"),
    )
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Query with both filters
    let dead_letters = app
        .fetch_dead_letters(
            DeadLetterQuery::new()
                .workflow_type("type_a")
                .workflow_id(WorkflowId::new("dlq-combo-a-1")),
        )
        .await?;
    assert_eq!(dead_letters.len(), 1);
    assert_eq!(
        dead_letters[0].workflow.workflow_id().as_str(),
        "dlq-combo-a-1"
    );

    // Query by type only
    let dead_letters = app
        .fetch_dead_letters(DeadLetterQuery::new().workflow_type("type_a"))
        .await?;
    assert_eq!(dead_letters.len(), 3);

    // Query by id only
    let dead_letters = app
        .fetch_dead_letters(DeadLetterQuery::new().workflow_id(WorkflowId::new("dlq-combo-b-1")))
        .await?;
    assert_eq!(dead_letters.len(), 1);

    Ok(())
});

db_test!(effect_with_corrupted_payload_dead_letters, |pool| {
    // Insert an effect with an invalid/corrupted payload that can't be deserialized
    db::insert_outbox_effect(
        pool,
        TestWorkflow::TYPE,
        "corrupted-effect-1",
        // Invalid effect payload - doesn't match TestWorkflowEffect structure
        serde_json::json!({"type": "NonexistentEffect", "invalid": true}),
        0,
        None,
    )
    .await?;

    let app = TestApp::builder(pool)
        .register(TestWorkflowHandler::new())
        .build_and_run()
        .await?;

    // Effect should fail deserialization and eventually dead-letter
    app.wait_for_dead_letter(
        DeadLetterQuery::new().workflow_id(WorkflowId::new("corrupted-effect-1")),
        DEFAULT_TEST_TIMEOUT,
    )
    .await?;

    Ok(())
});

db_test!(graceful_shutdown_completes_current_effect, |pool| {
    use std::sync::atomic::{AtomicBool, Ordering};

    let handler_started = Arc::new(AtomicBool::new(false));
    let handler_started_clone = handler_started.clone();

    struct SlowHandler {
        started: Arc<AtomicBool>,
        inner: TestWorkflowHandler,
    }

    #[async_trait]
    impl EffectHandler for SlowHandler {
        type Workflow = TestWorkflow;
        type Error = anyhow::Error;

        async fn handle(
            &self,
            effect: &TestWorkflowEffect,
            ctx: &EffectContext,
        ) -> Result<Option<TestWorkflowInput>, Self::Error> {
            self.started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.inner.handle(effect, ctx).await
        }
    }

    let app = TestApp::builder(pool)
        .register(SlowHandler {
            started: handler_started_clone,
            inner: TestWorkflowHandler::new(),
        })
        .build_and_run()
        .await?;

    app.service
        .execute::<TestWorkflow>(&TestWorkflowInput::start(
            "shutdown-test-1",
            EffectMode::RouteResult,
        ))
        .await?;

    // Wait for handler to start processing
    wait_until(DEFAULT_TEST_TIMEOUT, Duration::from_millis(10), || async {
        if handler_started.load(Ordering::SeqCst) {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    })
    .await?;

    // Shutdown should complete gracefully (effect finishes)
    let shutdown_timeout = Duration::from_secs(5);
    tokio::time::timeout(shutdown_timeout, app.shutdown())
        .await
        .expect("shutdown should complete within timeout")?;

    // Effect should have completed (not aborted mid-processing)
    let events = db::fetch_events(pool, TestWorkflow::TYPE, "shutdown-test-1").await?;
    assert_event_types(&events, &["Started", "Completed"]);

    Ok(())
});

db_test!(multiple_workers_no_double_processing, |pool| {
    init_test_tracing();

    let process_counts = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        u32,
    >::new()));
    let process_counts_clone = process_counts.clone();

    struct CountingHandler {
        counts: Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>,
        inner: TestWorkflowHandler,
    }

    #[async_trait]
    impl EffectHandler for CountingHandler {
        type Workflow = TestWorkflow;
        type Error = anyhow::Error;

        async fn handle(
            &self,
            effect: &TestWorkflowEffect,
            ctx: &EffectContext,
        ) -> Result<Option<TestWorkflowInput>, Self::Error> {
            let wf_id = ctx.workflow.workflow_id().to_string();
            {
                let mut counts = self.counts.lock().unwrap();
                *counts.entry(wf_id).or_insert(0) += 1;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.inner.handle(effect, ctx).await
        }
    }

    let app = TestApp::builder(pool)
        .register(CountingHandler {
            counts: process_counts_clone,
            inner: TestWorkflowHandler::new(),
        })
        .config(RuntimeConfig {
            effect_poll_interval: Duration::from_millis(10),
            effect_workers: 4,
            ..test_runtime_config()
        })
        .build_and_run()
        .await?;

    for i in 1..=5 {
        app.service
            .execute::<TestWorkflow>(&TestWorkflowInput::start(
                format!("no-double-{i}"),
                EffectMode::RouteResult,
            ))
            .await?;
    }

    let pool_ref = pool.clone();
    wait_until(DEFAULT_TEST_TIMEOUT, DEFAULT_POLL_INTERVAL, || async {
        let completed = db::count_completed_workflows(
            &pool_ref,
            TestWorkflow::TYPE,
            "no-double-%",
            "Completed",
        )
        .await?;
        if completed >= 5 {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    })
    .await?;

    let counts = process_counts.lock().unwrap();
    for i in 1..=5 {
        let wf_id = format!("no-double-{i}");
        let count = counts.get(&wf_id).copied().unwrap_or(0);
        assert_eq!(
            count, 1,
            "workflow {} should be processed exactly once, was processed {} times",
            wf_id, count
        );
    }

    Ok(())
});
