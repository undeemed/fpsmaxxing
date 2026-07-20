# FPSMaxxing documentation

Use this page as the documentation entry point. The Markdown documents are canonical; the HTML implementation plan is a static visualization of the same design.

## Core design

| Document | Purpose |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | Trust boundaries, services, data flow, and provider isolation |
| [Implementation plan](IMPLEMENTATION_PLAN.md) | Milestones, repository layout, scope, acceptance criteria, and handoff tasks |
| [Implementation plan visualization](IMPLEMENTATION_PLAN.html) | Static visual companion to the canonical Markdown plan |
| [Threat model](threat-model/README.md) | Assets, trust boundaries, threats, and required mitigations |

## Extension guides

| Document | Purpose |
| --- | --- |
| [Provider guide](providers/README.md) | Rules and lifecycle for provider implementations |
| [Rust-first ADR](adr/0001-rust-first.md) | Why the control plane uses Rust with one isolated .NET bridge |
| [Alpha journal ADR](adr/0002-alpha-experiment-journal.md) | Write-ahead apply intent, terminal outcomes, and deferred two-phase journaling |

Repository-wide contributor, security, support, and governance documents remain at the project root so GitHub can discover them automatically.
