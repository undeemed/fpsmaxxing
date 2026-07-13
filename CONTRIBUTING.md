# Contributing

FPSMaxxing welcomes focused, reviewable contributions that preserve the project's safety boundaries.

## Before opening a pull request

1. Read `AGENTS.md` and `docs/IMPLEMENTATION_PLAN.md`.
2. Open an issue before introducing a new privileged operation or changing a wire contract.
3. Keep provider-specific code inside its provider package.
4. Include negative-path and rollback tests for every state-changing operation.
5. Run the full validation suite documented in `AGENTS.md`.

## Pull request expectations

- Explain the user problem and why the change belongs in FPSMaxxing.
- List affected capabilities and risk classes.
- Describe pre-state capture, verification, lease expiry, and rollback.
- Document supported operating-system, provider, and hardware versions.
- Include evidence for any performance claim.

By submitting a contribution, you agree that it is licensed under Apache-2.0 as described in the repository license.
