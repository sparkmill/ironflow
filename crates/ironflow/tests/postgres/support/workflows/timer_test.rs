//! Timer test workflow for testing timer scheduling and cancellation mechanics.
//!
//! Tests framework timer functionality:
//! - Scheduling timers with `Timer::after()` and named keys
//! - Cancelling timers before they fire
//! - Timer-triggered inputs

use async_trait::async_trait;
use ironflow::{Decision, EffectContext, EffectHandler, HasWorkflowId, NonEmpty, Timer, Workflow};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub struct TimerTestWorkflow;

#[derive(Debug, Clone, Default)]
pub struct TimerTestState {
    pub started: bool,
    pub completed: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, HasWorkflowId)]
#[serde(tag = "type")]
#[workflow_id(id)]
pub enum TimerTestInput {
    /// Start workflow and schedule a timer.
    Start {
        id: String,
        /// Timer duration in seconds.
        timeout_secs: u64,
        /// Optional timer key for cancellation.
        timer_key: Option<String>,
    },
    /// Complete workflow (optionally cancel timer).
    Complete {
        id: String,
        /// Timer key to cancel (if any).
        cancel_timer_key: Option<String>,
    },
    /// Timer fired (routed from timer worker).
    Timeout { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TimerTestEvent {
    Started,
    Completed,
    TimedOut,
    TimerCancelled { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TimerTestEffect {
    /// Side effect that routes completion back.
    Process,
}

impl Workflow for TimerTestWorkflow {
    type State = TimerTestState;
    type Input = TimerTestInput;
    type Event = TimerTestEvent;
    type Effect = TimerTestEffect;

    const TYPE: &'static str = "timer_test";

    fn evolve(mut state: Self::State, event: Self::Event) -> Self::State {
        match event {
            TimerTestEvent::Started => state.started = true,
            TimerTestEvent::Completed => state.completed = true,
            TimerTestEvent::TimedOut => state.timed_out = true,
            TimerTestEvent::TimerCancelled { .. } => {}
        }
        state
    }

    fn decide(
        _now: OffsetDateTime,
        _state: &Self::State,
        input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input> {
        match input {
            TimerTestInput::Start {
                id,
                timeout_secs,
                timer_key,
            } => {
                let mut timer = Timer::after(
                    std::time::Duration::from_secs(*timeout_secs),
                    TimerTestInput::Timeout { id: id.clone() },
                );
                if let Some(key) = timer_key {
                    timer = timer.with_key(key);
                }
                Decision::event(TimerTestEvent::Started)
                    .with_effect(TimerTestEffect::Process)
                    .with_timer(timer)
            }
            TimerTestInput::Complete {
                cancel_timer_key, ..
            } => {
                if let Some(key) = cancel_timer_key {
                    let events = NonEmpty::collect(vec![
                        TimerTestEvent::Completed,
                        TimerTestEvent::TimerCancelled { key: key.clone() },
                    ])
                    .expect("non-empty");
                    Decision::from_events(events).cancel_timer(key)
                } else {
                    Decision::event(TimerTestEvent::Completed)
                }
            }
            TimerTestInput::Timeout { .. } => Decision::event(TimerTestEvent::TimedOut),
        }
    }
}

/// Handler that routes completion back to workflow.
#[derive(Default)]
pub struct TimerTestHandler;

#[async_trait]
impl EffectHandler for TimerTestHandler {
    type Workflow = TimerTestWorkflow;
    type Error = std::convert::Infallible;

    async fn handle(
        &self,
        _effect: &TimerTestEffect,
        ctx: &EffectContext,
    ) -> Result<Option<TimerTestInput>, Self::Error> {
        Ok(Some(TimerTestInput::Complete {
            id: ctx.workflow.workflow_id().to_string(),
            cancel_timer_key: Some("timeout".into()),
        }))
    }
}
