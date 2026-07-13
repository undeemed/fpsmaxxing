# Security policy

## Supported versions

FPSMaxxing is pre-release software. Only the latest commit on the default branch is considered for security fixes.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting feature for this repository. Do not create a public issue for vulnerabilities involving:

- privilege-boundary bypasses;
- arbitrary command, Registry, memory, MSR, PCI, EC, or firmware access;
- missing rollback or lease enforcement;
- sidecar impersonation or named-pipe authorization;
- unsafe fan, clock, power, or thermal behavior.

Include reproduction steps, the affected commit, impact, and any proposed mitigation. Please do not test against machines or accounts you do not own or control.

## Security design

The gateway is unprivileged. Mutations require a typed capability, policy approval, provider-specific bounds, a durable journal, verification, and independent rollback. See `docs/threat-model/README.md`.
