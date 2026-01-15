# Ironflow macros

`ironflow-macros` provides helper derives and utilities that simplify wiring workflows
into the Ironflow runtime. The main derive is `HasWorkflowId`, which infers each workflow
input's identifier field so the runtime can route events and timers reliably.

## Example

```rust
use ironflow_macros::HasWorkflowId;

#[derive(HasWorkflowId)]
#[workflow_id(order_id)]
enum OrderInput {
    Create { order_id: String, customer: String },
    Cancel { order_id: String },
}
```

The derive inspects the provided field and implements `HasWorkflowId` so you can safely
pass enums like `OrderInput` to the runtime without re-implementing ID plumbing.
