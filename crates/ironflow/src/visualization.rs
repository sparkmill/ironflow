//! State machine visualization for workflows.
//!
//! This module provides opt-in traits for visualizing workflow state machines.
//! Workflows can implement [`StateMachineView`] to expose their state structure
//! for debugging, monitoring dashboards, and user interfaces.
//!
//! # Example
//!
//! ```ignore
//! use ironflow::{Workflow, StateMachineView, StateInfo, TransitionInfo};
//!
//! impl StateMachineView for OrderWorkflow {
//!     fn state_info(state: &Self::State) -> StateInfo {
//!         let status = match state.status {
//!             OrderStatus::Created => "created",
//!             OrderStatus::Paid => "paid",
//!             OrderStatus::Shipped => "shipped",
//!             OrderStatus::Completed => "completed",
//!             OrderStatus::Cancelled => "cancelled",
//!         };
//!
//!         StateInfo::new(status)
//!             .with_field("order_id", &state.order_id)
//!             .with_field("total", &state.total)
//!             .terminal(matches!(
//!                 state.status,
//!                 OrderStatus::Completed | OrderStatus::Cancelled
//!             ))
//!     }
//!
//!     fn transitions(state: &Self::State) -> Vec<TransitionInfo> {
//!         match state.status {
//!             OrderStatus::Created => vec![
//!                 TransitionInfo::new("Pay", "paid")
//!                     .with_effect("SendPaymentConfirmation"),
//!                 TransitionInfo::new("Cancel", "cancelled"),
//!             ],
//!             OrderStatus::Paid => vec![
//!                 TransitionInfo::new("Ship", "shipped")
//!                     .with_effect("SendShippingNotification")
//!                     .with_timer("DeliveryCheck"),
//!             ],
//!             // Terminal states have no transitions
//!             _ => vec![],
//!         }
//!     }
//! }
//! ```

use std::collections::HashMap;

use crate::Workflow;

/// State machine visualization for a workflow.
///
/// Implement this trait to enable state machine introspection for debugging,
/// monitoring, and user interfaces. This is entirely opt-in.
///
/// # Design Notes
///
/// This trait is intentionally simple and doesn't require compile-time
/// state machine extraction. It's designed to be:
///
/// 1. **Easy to implement** - Just match on your status enum
/// 2. **Runtime-based** - Works with any state structure
/// 3. **Flexible** - You control what information to expose
///
/// For a more automatic approach, a derive macro could be added later
/// that generates this implementation from annotated state types.
pub trait StateMachineView: Workflow {
    /// Get information about the current state.
    ///
    /// This should return:
    /// - The current status/phase label
    /// - Key state fields for display
    /// - Whether this is a terminal state
    fn state_info(state: &Self::State) -> StateInfo;

    /// Get available transitions from the current state.
    ///
    /// Returns information about what inputs are valid in the current state
    /// and what effects/timers they would produce.
    ///
    /// This is a hint for UI/monitoring - the actual decision logic
    /// in `decide()` is the source of truth.
    fn transitions(state: &Self::State) -> Vec<TransitionInfo>;

    /// Get the complete state machine definition.
    ///
    /// Default implementation returns `None`. Override to provide
    /// a static graph representation for visualization tools.
    fn state_machine() -> Option<StateMachineDefinition> {
        None
    }
}

/// Information about the current workflow state.
#[derive(Debug, Clone)]
pub struct StateInfo {
    /// The current status/phase label (e.g., "created", "processing", "completed").
    pub status: String,

    /// Key-value pairs of state fields for display.
    pub fields: HashMap<String, FieldValue>,

    /// Whether this is a terminal (completed) state.
    pub is_terminal: bool,
}

impl StateInfo {
    /// Create a new state info with the given status.
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            fields: HashMap::new(),
            is_terminal: false,
        }
    }

    /// Add a field to display.
    pub fn with_field(mut self, name: impl Into<String>, value: &impl std::fmt::Display) -> Self {
        self.fields
            .insert(name.into(), FieldValue::String(value.to_string()));
        self
    }

    /// Add a numeric field.
    pub fn with_numeric_field(mut self, name: impl Into<String>, value: f64) -> Self {
        self.fields.insert(name.into(), FieldValue::Number(value));
        self
    }

    /// Add a boolean field.
    pub fn with_bool_field(mut self, name: impl Into<String>, value: bool) -> Self {
        self.fields.insert(name.into(), FieldValue::Bool(value));
        self
    }

    /// Mark this state as terminal.
    pub fn terminal(mut self, is_terminal: bool) -> Self {
        self.is_terminal = is_terminal;
        self
    }
}

/// A field value for display.
#[derive(Debug, Clone)]
pub enum FieldValue {
    String(String),
    Number(f64),
    Bool(bool),
}

impl std::fmt::Display for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldValue::String(s) => write!(f, "{}", s),
            FieldValue::Number(n) => write!(f, "{}", n),
            FieldValue::Bool(b) => write!(f, "{}", b),
        }
    }
}

/// Information about a possible transition from the current state.
#[derive(Debug, Clone)]
pub struct TransitionInfo {
    /// The input/action name that triggers this transition.
    pub input: String,

    /// The target status after this transition.
    pub target_status: String,

    /// Effects that would be produced by this transition.
    pub effects: Vec<String>,

    /// Timers that would be scheduled by this transition.
    pub timers: Vec<String>,

    /// Optional description for UI tooltips.
    pub description: Option<String>,
}

impl TransitionInfo {
    /// Create a new transition.
    pub fn new(input: impl Into<String>, target_status: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            target_status: target_status.into(),
            effects: vec![],
            timers: vec![],
            description: None,
        }
    }

    /// Add an effect that this transition produces.
    pub fn with_effect(mut self, effect: impl Into<String>) -> Self {
        self.effects.push(effect.into());
        self
    }

    /// Add a timer that this transition schedules.
    pub fn with_timer(mut self, timer: impl Into<String>) -> Self {
        self.timers.push(timer.into());
        self
    }

    /// Add a description for this transition.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Static state machine definition for visualization tools.
///
/// This provides the complete graph structure for rendering state diagrams
/// in tools like Graphviz, Mermaid, or custom UIs.
#[derive(Debug, Clone)]
pub struct StateMachineDefinition {
    /// All possible states in the state machine.
    pub states: Vec<StateDefinition>,

    /// All possible transitions between states.
    pub transitions: Vec<TransitionDefinition>,

    /// The initial state name.
    pub initial_state: String,
}

/// A state in the state machine definition.
#[derive(Debug, Clone)]
pub struct StateDefinition {
    /// The state name/label.
    pub name: String,

    /// Whether this is a terminal state.
    pub is_terminal: bool,

    /// Optional description.
    pub description: Option<String>,
}

impl StateDefinition {
    /// Create a new state definition.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_terminal: false,
            description: None,
        }
    }

    /// Mark as terminal state.
    pub fn terminal(mut self) -> Self {
        self.is_terminal = true;
        self
    }

    /// Add description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A transition in the state machine definition.
#[derive(Debug, Clone)]
pub struct TransitionDefinition {
    /// The source state name.
    pub from: String,

    /// The target state name.
    pub to: String,

    /// The input/action that triggers this transition.
    pub input: String,

    /// Effects produced by this transition.
    pub effects: Vec<String>,

    /// Timers scheduled by this transition.
    pub timers: Vec<String>,
}

impl TransitionDefinition {
    /// Create a new transition definition.
    pub fn new(from: impl Into<String>, to: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            input: input.into(),
            effects: vec![],
            timers: vec![],
        }
    }

    /// Add an effect.
    pub fn with_effect(mut self, effect: impl Into<String>) -> Self {
        self.effects.push(effect.into());
        self
    }

    /// Add a timer.
    pub fn with_timer(mut self, timer: impl Into<String>) -> Self {
        self.timers.push(timer.into());
        self
    }
}

impl StateMachineDefinition {
    /// Create a new state machine definition.
    pub fn new(initial_state: impl Into<String>) -> Self {
        Self {
            states: vec![],
            transitions: vec![],
            initial_state: initial_state.into(),
        }
    }

    /// Add a state.
    pub fn with_state(mut self, state: StateDefinition) -> Self {
        self.states.push(state);
        self
    }

    /// Add a transition.
    pub fn with_transition(mut self, transition: TransitionDefinition) -> Self {
        self.transitions.push(transition);
        self
    }

    /// Generate a Mermaid state diagram.
    ///
    /// Returns a string that can be rendered by Mermaid.js.
    ///
    /// # Example Output
    ///
    /// ```text
    /// stateDiagram-v2
    ///     [*] --> created
    ///     created --> paid : Pay
    ///     created --> cancelled : Cancel
    ///     paid --> shipped : Ship
    ///     shipped --> completed : Deliver
    ///     completed --> [*]
    ///     cancelled --> [*]
    /// ```
    pub fn to_mermaid(&self) -> String {
        let mut lines = vec!["stateDiagram-v2".to_string()];

        // Initial state arrow
        lines.push(format!("    [*] --> {}", self.initial_state));

        // Transitions
        for t in &self.transitions {
            let label = if t.effects.is_empty() && t.timers.is_empty() {
                t.input.clone()
            } else {
                let mut parts = vec![t.input.clone()];
                if !t.effects.is_empty() {
                    parts.push(format!("[{}]", t.effects.join(", ")));
                }
                if !t.timers.is_empty() {
                    parts.push(format!("⏱{}", t.timers.join(", ")));
                }
                parts.join(" ")
            };
            lines.push(format!("    {} --> {} : {}", t.from, t.to, label));
        }

        // Terminal states
        for s in &self.states {
            if s.is_terminal {
                lines.push(format!("    {} --> [*]", s.name));
            }
        }

        lines.join("\n")
    }

    /// Generate a DOT graph for Graphviz.
    ///
    /// # Example Output
    ///
    /// ```text
    /// digraph workflow {
    ///     rankdir=LR;
    ///     node [shape=box];
    ///
    ///     created -> paid [label="Pay"];
    ///     created -> cancelled [label="Cancel"];
    ///     paid -> shipped [label="Ship"];
    ///
    ///     completed [shape=doublecircle];
    ///     cancelled [shape=doublecircle];
    /// }
    /// ```
    pub fn to_dot(&self) -> String {
        let mut lines = vec![
            "digraph workflow {".to_string(),
            "    rankdir=LR;".to_string(),
            "    node [shape=box];".to_string(),
            "".to_string(),
        ];

        // Transitions
        for t in &self.transitions {
            let label = if t.effects.is_empty() && t.timers.is_empty() {
                t.input.clone()
            } else {
                let mut parts = vec![t.input.clone()];
                if !t.effects.is_empty() {
                    parts.push(format!("[{}]", t.effects.join(", ")));
                }
                if !t.timers.is_empty() {
                    parts.push(format!("timer:{}", t.timers.join(", ")));
                }
                parts.join("\\n")
            };
            lines.push(format!("    {} -> {} [label=\"{}\"];", t.from, t.to, label));
        }

        // Terminal states
        lines.push("".to_string());
        for s in &self.states {
            if s.is_terminal {
                lines.push(format!("    {} [shape=doublecircle];", s.name));
            }
        }

        lines.push("}".to_string());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_info_builder() {
        let info = StateInfo::new("processing")
            .with_field("order_id", &"ord-123")
            .with_numeric_field("total", 99.99)
            .with_bool_field("paid", true)
            .terminal(false);

        assert_eq!(info.status, "processing");
        assert_eq!(info.fields.len(), 3);
        assert!(!info.is_terminal);
    }

    #[test]
    fn transition_info_builder() {
        let transition = TransitionInfo::new("Ship", "shipped")
            .with_effect("SendShippingEmail")
            .with_timer("DeliveryCheck")
            .with_description("Ships the order");

        assert_eq!(transition.input, "Ship");
        assert_eq!(transition.target_status, "shipped");
        assert_eq!(transition.effects, vec!["SendShippingEmail"]);
        assert_eq!(transition.timers, vec!["DeliveryCheck"]);
    }

    #[test]
    fn state_machine_to_mermaid() {
        let sm = StateMachineDefinition::new("created")
            .with_state(StateDefinition::new("created"))
            .with_state(StateDefinition::new("paid"))
            .with_state(StateDefinition::new("completed").terminal())
            .with_transition(TransitionDefinition::new("created", "paid", "Pay"))
            .with_transition(
                TransitionDefinition::new("paid", "completed", "Complete")
                    .with_effect("SendReceipt"),
            );

        let mermaid = sm.to_mermaid();
        assert!(mermaid.contains("stateDiagram-v2"));
        assert!(mermaid.contains("[*] --> created"));
        assert!(mermaid.contains("created --> paid : Pay"));
        assert!(mermaid.contains("completed --> [*]"));
    }

    #[test]
    fn state_machine_to_dot() {
        let sm = StateMachineDefinition::new("created")
            .with_state(StateDefinition::new("created"))
            .with_state(StateDefinition::new("completed").terminal())
            .with_transition(TransitionDefinition::new(
                "created",
                "completed",
                "Complete",
            ));

        let dot = sm.to_dot();
        assert!(dot.contains("digraph workflow"));
        assert!(dot.contains("created -> completed"));
        assert!(dot.contains("[shape=doublecircle]"));
    }
}
