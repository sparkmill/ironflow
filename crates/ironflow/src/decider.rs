//! Workflow decision execution.

use time::OffsetDateTime;

use crate::error::Result;
use crate::store::{BeginResult, InputObservation, Store, UnitOfWork};
use crate::workflow::{HasWorkflowId, Workflow};

/// Execute a workflow decision.
///
/// This function:
/// 1. Extracts the workflow ID from the input
/// 2. Checks if workflow is already completed (silently returns if so)
/// 3. Begins a unit of work (acquires lock, loads events)
/// 4. Replays events to reconstruct current state
/// 5. Calls `Workflow::decide` with the current state and input
/// 6. Appends resulting events to the event store
/// 7. Enqueues resulting effects to the outbox
/// 8. Schedules any timers
/// 9. Marks workflow as completed if terminal state reached
/// 10. Commits the transaction
///
/// If the workflow is already completed, this function silently succeeds
/// (idempotent behavior). If any step fails, the transaction is rolled back
/// and no changes are persisted.
///
/// # Concurrency
///
/// Multiple executions for different workflow instances can run concurrently.
/// Executions for the same workflow instance are serialized by the store's
/// locking mechanism.
pub(crate) async fn execute<W, S>(
    store: &S,
    record_input_observations: bool,
    input: &W::Input,
) -> Result<()>
where
    W: Workflow,
    S: Store,
{
    let workflow_id = input.workflow_id();

    let (event_payloads, mut uow) = match store.begin(W::TYPE, &workflow_id).await? {
        BeginResult::Active { events, uow, .. } => (events, uow),
        BeginResult::Completed => {
            // Workflow already completed - silently succeed (idempotent)
            return Ok(());
        }
    };

    if record_input_observations {
        let payload = serde_json::to_value(input)?;
        let input_type = payload
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or(std::any::type_name::<W::Input>())
            .to_string();
        let observation = InputObservation {
            workflow_type: W::TYPE.to_string(),
            workflow_id: workflow_id.clone(),
            input_type,
            payload,
        };
        uow.record_input_observation(observation).await?;
    }

    let state = replay_state::<W>(W::TYPE, &workflow_id, event_payloads)?;

    let now = OffsetDateTime::now_utc();
    let decision = W::decide(now, &state, input);
    let (events, effects, timers, cancel_timers) = decision.into_parts();

    // Compute final state by applying new events
    let final_state = events.iter().cloned().fold(state, W::evolve);

    uow.append_events(events).await?;
    uow.enqueue_effects(effects).await?;

    if !cancel_timers.is_empty() {
        uow.cancel_timers(cancel_timers).await?;
    }

    // Convert timers to JSON for storage
    let json_timers: Vec<crate::Timer<serde_json::Value>> = timers
        .into_iter()
        .map(|t| {
            Ok(crate::Timer {
                fire_at: t.fire_at,
                input: serde_json::to_value(&t.input)?,
                key: t.key,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    uow.schedule_timers(json_timers).await?;

    // Check if workflow reached terminal state
    if W::is_terminal(&final_state) {
        uow.mark_completed();
    }

    uow.commit().await?;
    Ok(())
}

/// Replay events to reconstruct the current state.
fn replay_state<W: Workflow>(
    workflow_type: &'static str,
    workflow_id: &crate::WorkflowId,
    events: Vec<serde_json::Value>,
) -> Result<W::State> {
    let mut state = W::State::default();

    for (sequence, payload) in events.into_iter().enumerate() {
        let event: W::Event = serde_json::from_value(payload).map_err(|e| {
            crate::Error::event_deserialization(workflow_type, workflow_id.as_str(), sequence, e)
        })?;
        state = W::evolve(state, event);
    }

    Ok(state)
}
