//! Unified test workflow for testing ironflow framework mechanics.
//!
//! Supports testing:
//! - Basic event production (Ping → Pinged)
//! - Effect enqueueing and routing (Start with various modes)
//! - Terminal state detection (Stop → Stopped)
//! - Retry behavior (TransientFailure mode)
//! - Dead letter queue (PermanentFailure mode)

use anyhow::bail;
use async_trait::async_trait;
use ironflow::{Decision, EffectContext, EffectHandler, HasWorkflowId, Workflow};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub struct TestWorkflow;

#[derive(Debug, Clone, Default, PartialEq)]
pub enum TestWorkflowStatus {
    #[default]
    Idle,
    Processing,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TestWorkflowState {
    pub status: TestWorkflowStatus,
    pub value: Option<String>,
    pub counter: i32,
}

impl TestWorkflowState {
    pub fn is_terminal(&self) -> bool {
        self.status == TestWorkflowStatus::Stopped
    }
}

/// Controls how the effect handler behaves.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "mode")]
pub enum EffectMode {
    /// Effect succeeds, no result routing (fire-and-forget).
    #[default]
    FireAndForget,
    /// Effect succeeds and returns result for routing.
    RouteResult,
    /// Effect fails on first attempt, succeeds on retry.
    TransientFailure,
    /// Effect always fails (will eventually dead-letter).
    PermanentFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, HasWorkflowId)]
#[serde(tag = "type")]
#[workflow_id(id)]
pub enum TestWorkflowInput {
    /// Simple input that produces an event without any effect.
    Ping { id: String },
    /// Increment counter and optionally produce effect if counter > threshold.
    Increment { id: String, with_effect: bool },
    /// Start processing with the given mode (produces effect).
    Start { id: String, mode: EffectMode },
    /// Result routed back from effect handler.
    Completed { id: String, result: String },
    /// Failure routed back from effect handler.
    Failed { id: String, error: String },
    /// Stop the workflow (terminal state).
    Stop { id: String },
}

impl TestWorkflowInput {
    pub fn ping(id: impl Into<String>) -> Self {
        Self::Ping { id: id.into() }
    }

    pub fn increment(id: impl Into<String>) -> Self {
        Self::Increment {
            id: id.into(),
            with_effect: false,
        }
    }

    pub fn increment_with_effect(id: impl Into<String>) -> Self {
        Self::Increment {
            id: id.into(),
            with_effect: true,
        }
    }

    pub fn start(id: impl Into<String>, mode: EffectMode) -> Self {
        Self::Start {
            id: id.into(),
            mode,
        }
    }

    pub fn stop(id: impl Into<String>) -> Self {
        Self::Stop { id: id.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum TestWorkflowEvent {
    Pinged,
    Incremented { value: i32 },
    Started { mode: String },
    Completed { result: String },
    Failed { error: String },
    Stopped,
    EffectEnqueued { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TestWorkflowEffect {
    Process { mode: EffectMode },
    Notify { message: String },
}

impl Workflow for TestWorkflow {
    type State = TestWorkflowState;
    type Input = TestWorkflowInput;
    type Event = TestWorkflowEvent;
    type Effect = TestWorkflowEffect;

    const TYPE: &'static str = "test_workflow";

    fn evolve(mut state: Self::State, event: Self::Event) -> Self::State {
        match event {
            TestWorkflowEvent::Pinged => {}
            TestWorkflowEvent::Incremented { value } => {
                state.counter = value;
            }
            TestWorkflowEvent::Started { .. } => {
                state.status = TestWorkflowStatus::Processing;
            }
            TestWorkflowEvent::Completed { result } => {
                state.status = TestWorkflowStatus::Completed;
                state.value = Some(result);
            }
            TestWorkflowEvent::Failed { .. } => {
                state.status = TestWorkflowStatus::Failed;
            }
            TestWorkflowEvent::Stopped => {
                state.status = TestWorkflowStatus::Stopped;
            }
            TestWorkflowEvent::EffectEnqueued { .. } => {}
        }
        state
    }

    fn decide(
        _now: OffsetDateTime,
        state: &Self::State,
        input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input> {
        match input {
            TestWorkflowInput::Ping { .. } => Decision::event(TestWorkflowEvent::Pinged),

            TestWorkflowInput::Increment { with_effect, .. } => {
                let new_value = state.counter + 1;
                let decision = Decision::event(TestWorkflowEvent::Incremented { value: new_value });
                if *with_effect {
                    decision.with_effect(TestWorkflowEffect::Notify {
                        message: format!("Counter is now {}", new_value),
                    })
                } else {
                    decision
                }
            }

            TestWorkflowInput::Start { mode, .. } => {
                let mode_str = match mode {
                    EffectMode::FireAndForget => "fire_and_forget",
                    EffectMode::RouteResult => "route_result",
                    EffectMode::TransientFailure => "transient_failure",
                    EffectMode::PermanentFailure => "permanent_failure",
                };
                Decision::event(TestWorkflowEvent::Started {
                    mode: mode_str.into(),
                })
                .with_effect(TestWorkflowEffect::Process { mode: mode.clone() })
            }

            TestWorkflowInput::Completed { result, .. } => {
                Decision::event(TestWorkflowEvent::Completed {
                    result: result.clone(),
                })
            }

            TestWorkflowInput::Failed { error, .. } => Decision::event(TestWorkflowEvent::Failed {
                error: error.clone(),
            }),

            TestWorkflowInput::Stop { .. } => Decision::event(TestWorkflowEvent::Stopped),
        }
    }

    fn is_terminal(state: &Self::State) -> bool {
        state.is_terminal()
    }
}

/// Test handler with failure injection based on effect mode.
#[derive(Default)]
pub struct TestWorkflowHandler;

impl TestWorkflowHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EffectHandler for TestWorkflowHandler {
    type Workflow = TestWorkflow;
    type Error = anyhow::Error;

    async fn handle(
        &self,
        effect: &TestWorkflowEffect,
        ctx: &EffectContext,
    ) -> Result<Option<TestWorkflowInput>, Self::Error> {
        let id = ctx.workflow.workflow_id().to_string();

        match effect {
            TestWorkflowEffect::Process { mode } => match mode {
                EffectMode::FireAndForget => Ok(None),

                EffectMode::RouteResult => Ok(Some(TestWorkflowInput::Completed {
                    id,
                    result: "success".into(),
                })),

                EffectMode::TransientFailure => {
                    // Use ctx.attempt to validate runtime correctly tracks attempts
                    if ctx.attempt == 1 {
                        bail!("transient failure on attempt 1")
                    }
                    Ok(Some(TestWorkflowInput::Completed {
                        id,
                        result: format!("success on attempt {}", ctx.attempt),
                    }))
                }

                EffectMode::PermanentFailure => {
                    bail!("permanent failure")
                }
            },

            TestWorkflowEffect::Notify { .. } => {
                // Fire-and-forget notification, no routing
                Ok(None)
            }
        }
    }
}

// =============================================================================
// Effectless workflow for testing register_without_effects()
// =============================================================================

/// Minimal workflow that never emits effects.
/// Used to test `WorkflowBuilder::register_without_effects()`.
pub struct EffectlessWorkflow;

#[derive(Debug, Clone, Default, Serialize)]
pub struct EffectlessState {
    pub value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, HasWorkflowId)]
#[serde(tag = "type")]
#[workflow_id(id)]
pub enum EffectlessInput {
    Increment { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EffectlessEvent {
    Incremented { value: i32 },
}

impl Workflow for EffectlessWorkflow {
    type State = EffectlessState;
    type Input = EffectlessInput;
    type Event = EffectlessEvent;
    type Effect = (); // No effects

    const TYPE: &'static str = "effectless";

    fn evolve(mut state: Self::State, event: Self::Event) -> Self::State {
        match event {
            EffectlessEvent::Incremented { value } => state.value = value,
        }
        state
    }

    fn decide(
        _now: OffsetDateTime,
        state: &Self::State,
        _input: &Self::Input,
    ) -> Decision<Self::Event, Self::Effect, Self::Input> {
        Decision::event(EffectlessEvent::Incremented {
            value: state.value + 1,
        })
    }
}
