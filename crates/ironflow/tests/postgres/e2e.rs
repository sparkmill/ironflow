//! End-to-end integration scenarios.
//!
//! These tests verify complete workflow lifecycles that require timer scheduling
//! and cancellation. Effect processing and retry behavior tests are in runtime.rs.

use ironflow::Workflow;
use test_utils::db_test;

use crate::support::helpers::{DEFAULT_TEST_TIMEOUT, TestApp, assert_event_types};
use crate::support::workflows::timer_test::{TimerTestHandler, TimerTestInput, TimerTestWorkflow};

db_test!(timer_scheduling_and_cancellation, |pool| {
    let app = TestApp::builder(pool)
        .register(TimerTestHandler)
        .build_and_run()
        .await?;

    // Start workflow with timer scheduling (60 second timeout)
    app.service
        .execute::<TimerTestWorkflow>(&TimerTestInput::Start {
            id: "timer-test-1".into(),
            timeout_secs: 60,
            timer_key: Some("timeout".into()),
        })
        .await?;

    // Effect handler routes completion back, which cancels the timer
    let events = app
        .wait_for_events(
            TimerTestWorkflow::TYPE,
            "timer-test-1",
            3,
            DEFAULT_TEST_TIMEOUT,
        )
        .await?;

    assert_event_types(&events, &["Started", "Completed", "TimerCancelled"]);

    // Verify timer was cancelled (not pending)
    let pending_timers = app
        .count_pending_timers(TimerTestWorkflow::TYPE, "timer-test-1", Some("timeout"))
        .await?;
    assert_eq!(pending_timers, 0);

    Ok(())
});
