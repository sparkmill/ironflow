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
