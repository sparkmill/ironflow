# RFC: Dead-letter propagation into workflows

## Problem

When an effect exceeds `retry_policy.max_attempts`, the effect worker calls
`outbox.record_permanent_failure()` and stops. The effect's workflow instance
is not notified: no `Input` is dispatched, no event is appended, no state
transition happens.

From the workflow's point of view, nothing happened. The state machine sits
forever in whatever state emitted the effect (e.g. `Submitting`, `Polling`),
waiting for an input that will never arrive.

Concrete sighting (2026-04-23, unity service):

```
WARN ironflow::runtime::effect_worker: Effect exceeded max retries,
    moving to dead letter, effect_id: 019d…7007,
    error: traject API error (status 400): {…"Collection cannot be run as it
    has no Requests"…}, attempts: 5, max_attempts: 5
```

The owning `TrajectIngestionWorkflow` stayed in `Submitting` indefinitely.
UI cards polling `get_status` showed "Submitting" forever; operator had to
manually cancel via the `Cancel` input path to clear it.

### Why this matters

Ironflow's appeal is that it encodes failure as a first-class transition
(`EffectFailed → Failed { reason }`). Workflows are already required to
handle it — every sample workflow we ship has an `EffectFailed` arm in
`decide`. But the runtime never actually sends that input on DL, so the
branch only fires for *non-DL* failure paths (e.g. handler returns an input
like `EffectFailed` itself, which isn't a pattern any of our services
currently use).

The result is that "give up after N retries" silently turns into "strand
the workflow." That is the opposite of what the DL feature is meant to
signal.

### Current mitigations (all bad)

| Mitigation | Why it hurts |
|---|---|
| Set `max_attempts = 1` for effects that shouldn't retry | Loses retry for transient errors in the same code path |
| Poll `fetch_dead_letters()` from the app and dispatch a synthetic `EffectFailed` | Every user re-implements the same reconciler. Racy. Needs per-workflow-type routing tables in app code. |
| Manually `retry_dead_letter(id)` until exhaustion | Requires human intervention; doesn't scale; by definition retry won't help if the root cause is permanent |
| Cancel + restart the workflow | Loses any partial progress; requires operator knowledge of how to find stuck instances |

## Proposed design (sketch — details TBD)

Make dead-lettering a **synchronous terminal signal** to the workflow:
right after `record_permanent_failure`, the effect worker dispatches an
input back into the decider loop, carrying the final error message.

Two routes worth evaluating:

### Route A — New built-in input variant

Add an `Input::EffectDeadLettered { effect_id, error }` variant that users
opt into by implementing a method on the `Workflow` trait:

```rust
trait Workflow {
    // … existing items …

    /// Construct the input to dispatch when an effect for this workflow
    /// exhausts its retry policy. Default: synthesize a rejection so the
    /// workflow completes with a clear audit record instead of stranding.
    fn on_dead_letter(effect_id: Uuid, error: &str) -> Self::Input {
        // default requires Input to have a blanket EffectDeadLettered
        // variant — see route B for an alternative that doesn't
    }
}
```

Pros: explicit, per-workflow override (some workflows might want to
re-enqueue; others terminate). Cons: requires a new trait method and a
known-shape input variant — user breakage on upgrade.

### Route B — Treat DL as a special effect-completion path

When DL'd, the runtime invokes the `EffectHandler`'s existing contract
with a sentinel `Result::Err` type (or a new `HandleOutcome::DeadLetter`),
and the handler's existing error-to-input translation runs. Users who
don't care get today's stranded behavior behind a feature flag; users
who do care return `Some(Input::EffectFailed { ... })`.

Pros: no new trait method, works with the shape users already have.
Cons: conflates "transient retryable error" and "permanent DL" at the
handler-error boundary unless we widen the return type.

### Route C — Subscription-based reconciler in the runtime

Ship a built-in `DeadLetterReconciler` task as part of `WorkflowRuntime`
that polls the DL store and, for each new entry, dispatches a
user-provided `Input` via a registered closure per workflow type.

Pros: backwards compatible, no trait changes, turn on with a runtime
builder call. Cons: inherently polling-based unless we add a notify
channel from the outbox store.

## Requirements for the eventual design

Whatever route we pick, the solution must:

1. **Guarantee termination of stranded workflows** — no state machine
   should be able to end up with all inputs exhausted while the runtime
   considers it "live." DL without propagation breaks this guarantee.

2. **Preserve the error detail at the workflow level** — the final
   error string that landed the effect in DL should be available to
   `decide` so it can populate a `Failed { reason }` event. Without this,
   the audit trail points at an effect id the operator has to manually
   look up.

3. **Play nicely with `unique_key` / singleton workflows** — the unity
   `TrajectIngestionWorkflow` is a singleton. A stranded instance blocks
   all future `Start`s until it's manually cancelled. Termination on DL
   must release the uniqueness lock as a natural consequence.

4. **Not introduce retry loops** — the whole point of DL is "stop
   trying." The propagation path must not re-enqueue the effect or
   trigger the retry policy a second time.

5. **Be opt-in and observable at the runtime level** — workflows that
   *want* to stay stuck on DL (for manual operator review) should be
   able to. Default should be safe-by-default termination with a log.

6. **Mark the DL entry as "handled"** — so the reconciler / next
   invocation doesn't re-dispatch the same `EffectFailed` repeatedly.
   Today `fetch_dead_letters` returns the full set each call; there's no
   "mark processed" counterpart without `retry_dead_letter`, which is
   semantically different.

## Timer dead-letters

Note from `effect_worker.rs` sibling code — timers have the same DL
mechanism (`fetch_timer_dead_letters`). Any design landed here should
apply to both paths, or explicitly scope one and document the gap.

## Open questions

- Should `on_dead_letter` be able to return `None` to mean "stay stuck"?
  If so, that's equivalent to today's behavior and probably not worth
  the ceremony.
- When the effect that DL'd originally produced a `Some(Input::…)` on
  its success path, should the failure input share the same routing
  semantics (goes back into `execute`), or a simpler "write Failed event
  directly"?
- How does this interact with in-progress work on projections? A
  `Failed` event emitted by the runtime has to flow through projections
  just like user-emitted events; confirm.
- `unique_key` release: if termination happens via a synthesized event,
  the lock clears normally. If via direct state mutation, does it?

## Non-goals

- Automatic retry of DL'd effects once the root cause is fixed — that's
  the existing `retry_dead_letter(id)` API.
- A UI / admin surface for DL inspection — separate feature.
- Changing retry policy semantics — the trigger for DL stays the same.

## References

- `runtime/effect_worker.rs:195-215` — current DL write path (no input
  dispatch).
- `runtime/registry.rs:694-735` — DL query + manual retry API.
- Downstream workflow affected (example of the strand):
  `red-pill/crates/platform/amazon-intel/src/enrich_market/traject_ingestion/workflow.rs`
  — the `EffectFailed` arm of `decide` is written but unreachable on DL.
