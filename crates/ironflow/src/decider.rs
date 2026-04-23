//! Workflow decision execution.

use time::OffsetDateTime;

use crate::error::Result;
use crate::service::ExecuteOutcome;
use crate::store::{BeginResult, InputObservation, ObservationOutcome, Store, UnitOfWork};
use crate::workflow::{Decision, HasWorkflowId, Workflow};

/// Execute a workflow decision.
///
/// This function:
/// 1. Extracts the workflow ID from the input.
/// 2. Begins a unit of work (acquires lock, loads events).
/// 3. If the workflow is already completed, records an observation (if
///    enabled) and returns `AlreadyCompleted` — input is not dispatched.
/// 4. Replays events to reconstruct current state.
/// 5. Calls `Workflow::decide` with the current state and input.
/// 6. On `Decision::Accept { .. }`: appends events, enqueues effects,
///    schedules/cancels timers, marks terminal if applicable, records the
///    accepted-input observation (if enabled), commits.
/// 7. On `Decision::Reject(r)`: rolls back the unit of work (dropping any
///    newly-inserted `workflow_instances` row so rejected bootstraps don't
///    leave ghosts) and records a standalone rejected-input observation
///    (if enabled). Returns `Rejected(r)`.
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
) -> Result<ExecuteOutcome<W::Rejection>>
where
    W: Workflow,
    S: Store,
{
    let workflow_id = input.workflow_id();
    let unique_key = W::unique_key(input);

    let (event_payloads, mut uow) = match store
        .begin(W::TYPE, &workflow_id, unique_key.as_deref())
        .await?
    {
        BeginResult::Active { events, uow, .. } => (events, uow),
        BeginResult::Completed => {
            // Workflow already completed — record observation (if enabled)
            // and signal to the caller that the input was not dispatched.
            if record_input_observations {
                let payload = serde_json::to_value(input)?;
                let input_type = extract_input_type::<W>(&payload);
                let observation = InputObservation {
                    workflow_type: W::TYPE.to_string(),
                    workflow_id: workflow_id.clone(),
                    input_type,
                    payload,
                    outcome: ObservationOutcome::AlreadyCompleted,
                };
                store.record_observation(observation).await?;
            }
            return Ok(ExecuteOutcome::AlreadyCompleted);
        }
    };

    let state = replay_state::<W>(W::TYPE, &workflow_id, event_payloads)?;

    let now = OffsetDateTime::now_utc();
    let decision = W::decide(now, &state, input);

    match decision {
        Decision::Accept {
            events,
            effects,
            timers,
            cancel_timers,
        } => {
            if record_input_observations {
                let payload = serde_json::to_value(input)?;
                let input_type = extract_input_type::<W>(&payload);
                let observation = InputObservation {
                    workflow_type: W::TYPE.to_string(),
                    workflow_id: workflow_id.clone(),
                    input_type,
                    payload,
                    outcome: ObservationOutcome::Accepted,
                };
                uow.record_input_observation(observation).await?;
            }

            // Compute final state by applying new events
            let final_state = events.iter().cloned().fold(state, W::evolve);
            let events_appended = events.len();

            uow.append_events(events).await?;
            uow.enqueue_effects(effects).await?;

            if !cancel_timers.is_empty() {
                uow.cancel_timers(cancel_timers).await?;
            }

            // Convert timer inputs to JSON for storage. Delay and key
            // pass through unchanged; fire_at is computed DB-side.
            let json_timers: Vec<crate::Timer<serde_json::Value>> = timers
                .into_iter()
                .map(|t| {
                    Ok(crate::Timer {
                        delay: t.delay,
                        input: serde_json::to_value(&t.input)?,
                        key: t.key,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            uow.schedule_timers(json_timers).await?;

            if W::is_terminal(&final_state) {
                uow.mark_completed();
            }

            uow.commit().await?;
            Ok(ExecuteOutcome::Accepted { events_appended })
        }
        Decision::Reject(rejection) => {
            // Drop the unit of work without committing — this rolls back any
            // newly-inserted workflow_instances row so a rejected bootstrap
            // doesn't leave a ghost. Existing instances are untouched because
            // we never modified them beyond the lock.
            drop(uow);

            if record_input_observations {
                let payload = serde_json::to_value(input)?;
                let input_type = extract_input_type::<W>(&payload);
                let rejection_payload = serde_json::to_value(&rejection)?;
                let observation = InputObservation {
                    workflow_type: W::TYPE.to_string(),
                    workflow_id: workflow_id.clone(),
                    input_type,
                    payload,
                    outcome: ObservationOutcome::Rejected(rejection_payload),
                };
                store.record_observation(observation).await?;
            }

            Ok(ExecuteOutcome::Rejected(rejection))
        }
    }
}

/// Replay events to reconstruct the current state.
///
/// `events` is the full stream from the store in sequence order, starting at
/// per-workflow sequence 1. The Nth event (0-indexed) has sequence `N + 1`,
/// which is what we report in error context — matches the `sequence` column
/// in the event store, unlike the iteration index.
fn replay_state<W: Workflow>(
    workflow_type: &'static str,
    workflow_id: &crate::WorkflowId,
    events: Vec<serde_json::Value>,
) -> Result<W::State> {
    let mut state = W::State::default();

    for (index, payload) in events.into_iter().enumerate() {
        let sequence = (index as i64) + 1;
        let event: W::Event = serde_json::from_value(payload).map_err(|e| {
            crate::Error::event_deserialization(workflow_type, workflow_id.as_str(), sequence, e)
        })?;
        state = W::evolve(state, event);
    }

    Ok(state)
}

/// Best-effort input type name derivation for observations.
///
/// Prefers the `"type"` tag from the serialized input (common with
/// `#[serde(tag = "type")]` enums); falls back to Rust's `type_name`.
fn extract_input_type<W: Workflow>(payload: &serde_json::Value) -> String {
    payload
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or(std::any::type_name::<W::Input>())
        .to_string()
}
