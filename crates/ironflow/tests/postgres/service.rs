//! Tests for WorkflowService and WorkflowBuilder.
//!
//! These tests verify:
//! - Builder configuration and workflow registration
//! - Service execution (typed and dynamic routing)
//! - Event replay and state reconstruction
//! - Effect enqueueing
//! - Terminal state handling
//! - Input observation recording

use crate::db_test;
use ironflow::runtime::{RuntimeConfig, WorkflowRuntime};
use ironflow::{Error, PgStore, Workflow, WorkflowId, WorkflowServiceConfig};

use crate::support::db::{
    count_events, fetch_effects, fetch_events, fetch_input_observations, seed_events,
};
use crate::support::helpers::assert_event_types;
use crate::support::workflows::test_workflow::{
    EffectlessInput, EffectlessWorkflow, TestWorkflow, TestWorkflowEvent, TestWorkflowHandler,
    TestWorkflowInput,
};

fn build_service(
    store: PgStore,
    record_input_observations: bool,
) -> ironflow::WorkflowService<PgStore> {
    let config = WorkflowServiceConfig {
        record_input_observations,
    };
    WorkflowRuntime::builder(store, config)
        .register(TestWorkflowHandler::new())
        .build_service()
        .expect("service should build")
}

// =============================================================================
// Builder tests
// =============================================================================

db_test!(builder_creates_runtime, |pool| {
    let store = PgStore::new(pool.clone());
    let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .build_runtime()
        .expect("should build runtime with one handler");

    assert_eq!(runtime.workflow_count(), 1);
    assert!(!runtime.worker_id().is_empty());
    Ok(())
});

db_test!(builder_with_custom_config, |pool| {
    let store = PgStore::new(pool.clone());
    let config = RuntimeConfig {
        worker_id: Some("test-worker".to_string()),
        ..Default::default()
    };

    let runtime = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .config(config)
        .build_runtime()
        .expect("should build runtime with custom config");

    assert_eq!(runtime.worker_id(), "test-worker");
    Ok(())
});

db_test!(builder_rejects_duplicate_registration, |pool| {
    let store = PgStore::new(pool.clone());
    let result = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .register(TestWorkflowHandler::new())
        .build_runtime();
    assert!(matches!(
        result,
        Err(ironflow::Error::DuplicateWorkflowType(_))
    ));
    Ok(())
});

db_test!(register_populates_both_dispatch_paths, |pool| {
    // A single .register(handler) call must populate BOTH the string-keyed
    // dynamic registry and the TypeId-keyed typed registry. If either map
    // were skipped, one of the execute paths below would return
    // Error::UnknownWorkflowType (propagated through the `?`), and the
    // test would fail before the matches! check. This guards against a
    // future PR that modifies registration and forgets one of the two
    // inserts.
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    // Typed path routes through WorkflowRegistry::get_typed (TypeId-keyed).
    let typed = service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("dual-path-typed"))
        .await?;
    assert!(
        matches!(typed, ironflow::ExecuteOutcome::Accepted { .. }),
        "typed path should dispatch after registration, got {typed:?}"
    );

    // Dynamic path routes through WorkflowRegistry::get (string-keyed).
    let dynamic_input = serde_json::json!({
        "type": "Ping",
        "id": "dual-path-dynamic",
    });
    let dynamic = service
        .execute_dynamic(TestWorkflow::TYPE, &dynamic_input)
        .await?;
    assert!(
        matches!(dynamic, ironflow::ExecuteOutcome::Accepted { .. }),
        "dynamic path should dispatch after registration, got {dynamic:?}"
    );

    Ok(())
});

db_test!(builder_register_without_effects, |pool| {
    use crate::support::workflows::test_workflow::{EffectlessInput, EffectlessWorkflow};

    let store = PgStore::new(pool.clone());
    let service = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
        .register_without_effects::<EffectlessWorkflow>()
        .build_service()?;

    // Execute workflow - should work without any effect handler
    service
        .execute::<EffectlessWorkflow>(&EffectlessInput::Increment {
            id: "effectless-1".into(),
        })
        .await?;

    // Verify event was produced
    let events = fetch_events(pool, EffectlessWorkflow::TYPE, "effectless-1").await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "Incremented");
    assert_eq!(events[0]["value"], 1);

    // Verify no effects were enqueued
    let effects = fetch_effects(pool, EffectlessWorkflow::TYPE, "effectless-1").await?;
    assert!(effects.is_empty());

    Ok(())
});

// =============================================================================
// Dynamic routing tests
// =============================================================================

db_test!(execute_dynamic_unknown_workflow_fails, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    let input = serde_json::json!({
        "type": "Ping",
        "id": "test-1"
    });

    let result = service.execute_dynamic("nonexistent", &input).await;
    assert!(matches!(result, Err(Error::UnknownWorkflowType(_))));
    Ok(())
});

db_test!(execute_dynamic_routes_to_workflow, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    let input = serde_json::json!({
        "type": "Ping",
        "id": "test-1"
    });

    service.execute_dynamic(TestWorkflow::TYPE, &input).await?;
    let count = count_events(pool).await?;
    assert_eq!(count, 1);
    Ok(())
});

db_test!(execute_typed_routes_to_workflow, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;

    let count = count_events(pool).await?;
    assert_eq!(count, 1);
    Ok(())
});

// =============================================================================
// Service execution tests
// =============================================================================

db_test!(execute_on_new_workflow, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment("test-1"))
        .await?;

    let events = fetch_events(pool, TestWorkflow::TYPE, "test-1").await?;
    let effects = fetch_effects(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_event_types(&events, &["Incremented"]);
    assert!(effects.is_empty());
    Ok(())
});

db_test!(execute_replays_existing_events, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store.clone(), false);
    let workflow_id = WorkflowId::new("test-1");
    let existing_events = vec![
        TestWorkflowEvent::Incremented { value: 1 },
        TestWorkflowEvent::Incremented { value: 2 },
    ];

    seed_events(&store, TestWorkflow::TYPE, &workflow_id, existing_events).await?;

    // State: counter = 2, incrementing again → counter = 3 with effect
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment_with_effect("test-1"))
        .await?;

    let events = fetch_events(pool, TestWorkflow::TYPE, "test-1").await?;
    let effects = fetch_effects(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_event_types(&events, &["Incremented", "Incremented", "Incremented"]);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["message"], "Counter is now 3");
    Ok(())
});

db_test!(execute_enqueues_effects, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment_with_effect("test-1"))
        .await?;

    let effects = fetch_effects(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_eq!(effects.len(), 1);
    let effect = &effects[0];
    assert_eq!(effect["type"], "Notify");
    assert_eq!(effect["message"], "Counter is now 1");
    Ok(())
});

db_test!(execute_detects_terminal_state, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::stop("test-1"))
        .await?;

    let events = fetch_events(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_event_types(&events, &["Pinged", "Stopped"]);
    Ok(())
});

db_test!(execute_skips_completed_workflow, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::stop("test-1"))
        .await?;

    let event_count = fetch_events(pool, TestWorkflow::TYPE, "test-1")
        .await?
        .len();

    // This should be skipped since workflow is stopped (terminal)
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;

    let events = fetch_events(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_eq!(events.len(), event_count);
    Ok(())
});

db_test!(execute_records_input_observation_when_enabled, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, true);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;

    let observations = fetch_input_observations(pool, TestWorkflow::TYPE, "test-1").await?;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0, "Ping");
    assert_eq!(observations[0].1["type"], "Ping");
    Ok(())
});

// =============================================================================
// Query APIs
// =============================================================================

db_test!(list_workflows_filters_by_type, |pool| {
    let store = PgStore::new(pool.clone());
    let service = WorkflowRuntime::builder(store, WorkflowServiceConfig::default())
        .register(TestWorkflowHandler::new())
        .register_without_effects::<EffectlessWorkflow>()
        .build_service()?;

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("test-2"))
        .await?;
    service
        .execute::<EffectlessWorkflow>(&EffectlessInput::Increment {
            id: "effectless-1".into(),
        })
        .await?;

    let all = service.list_workflows(None, 10, 0).await?;
    assert_eq!(all.len(), 3);

    let test_only = service
        .list_workflows(Some(TestWorkflow::TYPE), 10, 0)
        .await?;
    assert_eq!(test_only.len(), 2);
    assert!(
        test_only
            .iter()
            .all(|workflow| workflow.workflow_type == TestWorkflow::TYPE)
    );

    Ok(())
});

db_test!(fetch_workflow_events_returns_history, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);
    let workflow_id = WorkflowId::new("history-1");

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment("history-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment("history-1"))
        .await?;

    let events = service
        .fetch_workflow_events(TestWorkflow::TYPE, &workflow_id)
        .await?;

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].sequence, 2);
    assert_eq!(events[0].payload["type"], "Incremented");
    assert_eq!(events[1].payload["type"], "Incremented");
    Ok(())
});

db_test!(fetch_latest_state_returns_json, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);
    let workflow_id = WorkflowId::new("state-1");

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment("state-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment("state-1"))
        .await?;

    let state = service
        .fetch_latest_state(TestWorkflow::TYPE, &workflow_id)
        .await?;

    assert_eq!(state["counter"].as_i64(), Some(2));
    Ok(())
});

// =============================================================================
// Outcome / rejection tests
// =============================================================================

db_test!(execute_returns_accepted_with_event_count, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    let outcome = service
        .execute::<TestWorkflow>(&TestWorkflowInput::increment_with_effect("accept-1"))
        .await?;

    assert!(
        matches!(
            outcome,
            ironflow::ExecuteOutcome::Accepted { events_appended: 1 }
        ),
        "expected Accepted {{ events_appended: 1 }}, got {outcome:?}"
    );
    Ok(())
});

db_test!(execute_returns_rejected_with_typed_reason, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    // Bootstrap then reject a subsequent input
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("reject-1"))
        .await?;

    let outcome = service
        .execute::<TestWorkflow>(&TestWorkflowInput::force_reject(
            "reject-1",
            "validation failed",
        ))
        .await?;

    match outcome {
        ironflow::ExecuteOutcome::Rejected(reason) => {
            assert_eq!(reason, std::borrow::Cow::Borrowed("validation failed"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    Ok(())
});

db_test!(rejected_input_does_not_append_events, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("no-events-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::force_reject("no-events-1", "nope"))
        .await?;

    let events = fetch_events(pool, TestWorkflow::TYPE, "no-events-1").await?;
    // Only the Ping event; the rejected input did not append anything
    assert_event_types(&events, &["Pinged"]);
    Ok(())
});

db_test!(rejected_bootstrap_rolls_back_instance_row, |pool| {
    // Reject the very first input → no workflow_instances row should remain.
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    let outcome = service
        .execute::<TestWorkflow>(&TestWorkflowInput::force_reject(
            "bootstrap-reject-1",
            "not allowed",
        ))
        .await?;

    assert!(matches!(outcome, ironflow::ExecuteOutcome::Rejected(_)));

    let instance_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ironflow.workflow_instances
         WHERE workflow_type = $1 AND workflow_id = $2",
        TestWorkflow::TYPE,
        "bootstrap-reject-1",
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);

    assert_eq!(
        instance_count, 0,
        "rejected bootstrap should leave no workflow_instances row"
    );
    Ok(())
});

db_test!(rejection_persisted_when_observations_enabled, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, true);

    // Bootstrap first so the workflow exists
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("obs-reject-1"))
        .await?;

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::force_reject("obs-reject-1", "boom"))
        .await?;

    let row = sqlx::query!(
        r#"SELECT outcome, rejection_payload
           FROM ironflow.input_observations
           WHERE workflow_id = $1 AND outcome = 'rejected'"#,
        "obs-reject-1",
    )
    .fetch_one(pool)
    .await?;

    assert_eq!(row.outcome, "rejected");
    assert_eq!(row.rejection_payload.unwrap(), serde_json::json!("boom"));
    Ok(())
});

db_test!(rejection_not_persisted_when_observations_disabled, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("obs-off-1"))
        .await?;
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::force_reject("obs-off-1", "quiet"))
        .await?;

    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ironflow.input_observations WHERE workflow_id = $1",
        "obs-off-1",
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);

    assert_eq!(count, 0, "no observations expected when flag is off");
    Ok(())
});

db_test!(
    execute_returns_already_completed_for_terminal_workflow,
    |pool| {
        let store = PgStore::new(pool.clone());
        let service = build_service(store, false);

        service
            .execute::<TestWorkflow>(&TestWorkflowInput::ping("done-1"))
            .await?;
        service
            .execute::<TestWorkflow>(&TestWorkflowInput::stop("done-1"))
            .await?;

        let outcome = service
            .execute::<TestWorkflow>(&TestWorkflowInput::ping("done-1"))
            .await?;

        assert!(
            matches!(outcome, ironflow::ExecuteOutcome::AlreadyCompleted),
            "expected AlreadyCompleted, got {outcome:?}"
        );
        Ok(())
    }
);

db_test!(
    already_completed_persisted_when_observations_enabled,
    |pool| {
        let store = PgStore::new(pool.clone());
        let service = build_service(store, true);

        service
            .execute::<TestWorkflow>(&TestWorkflowInput::ping("ac-obs-1"))
            .await?;
        service
            .execute::<TestWorkflow>(&TestWorkflowInput::stop("ac-obs-1"))
            .await?;
        service
            .execute::<TestWorkflow>(&TestWorkflowInput::ping("ac-obs-1"))
            .await?;

        let row = sqlx::query!(
            r#"SELECT outcome
           FROM ironflow.input_observations
           WHERE workflow_id = $1 AND outcome = 'already_completed'"#,
            "ac-obs-1",
        )
        .fetch_one(pool)
        .await?;

        assert_eq!(row.outcome, "already_completed");
        Ok(())
    }
);

db_test!(execute_dynamic_returns_json_rejection, |pool| {
    let store = PgStore::new(pool.clone());
    let service = build_service(store, false);

    // Bootstrap
    service
        .execute::<TestWorkflow>(&TestWorkflowInput::ping("dyn-reject-1"))
        .await?;

    let input = serde_json::json!({
        "type": "ForceReject",
        "id": "dyn-reject-1",
        "reason": "from-dynamic",
    });
    let outcome = service.execute_dynamic(TestWorkflow::TYPE, &input).await?;

    match outcome {
        ironflow::ExecuteOutcome::Rejected(payload) => {
            assert_eq!(payload, serde_json::json!("from-dynamic"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    Ok(())
});
