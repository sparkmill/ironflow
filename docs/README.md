# Ironflow Documentation

This folder describes the target Ironflow design and how it runs many
workflow types and their instances in a single runtime.

## Contents

| Document                             | Description                                         |
| ------------------------------------ | --------------------------------------------------- |
| [ARCHITECTURE.md](ARCHITECTURE.md)   | Single source of truth for the target design        |
| [WORKFLOW_CORE.md](WORKFLOW_CORE.md) | Core workflow execution model (short and practical) |
| [HOWTO.md](HOWTO.md)                 | Practical guides for common patterns                |
| [PROJECTIONS.md](PROJECTIONS.md)     | Building read models from event streams             |
| [DEVELOPMENT.md](DEVELOPMENT.md)     | Local setup, tooling, and test utilities            |
| [PUBLISHING.md](PUBLISHING.md)       | Release process for publishing crates               |

## Recommended Reading Order

1. ARCHITECTURE.md — system overview, workflow model, runtime, storage, guarantees
2. WORKFLOW_CORE.md — core execution flow, invariants, minimal example
3. HOWTO.md — idempotency, timers, effects, unique keys

## See Also

- [CONTRIBUTING.md](../CONTRIBUTING.md) — PR checklist, coding style, checks
- [CHANGELOG.md](../CHANGELOG.md) — release history and migration notes
