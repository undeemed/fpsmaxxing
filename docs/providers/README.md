# Provider development

Provider sidecars are capability adapters, not autonomous optimizers.

Each provider must document:

- supported provider and OS versions;
- discovery and health checks;
- semantic capabilities and parameter bounds;
- required privilege;
- persistence and reboot behavior;
- conflicts and ownership;
- snapshot format;
- side-effect-free preview text;
- verification probe;
- rollback behavior;
- test fixtures and failure injection.

Start new providers from `sidecars/mock-provider`.
