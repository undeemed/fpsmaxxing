# LibreHardwareMonitor bridge

This directory is reserved for the only planned non-Rust component in v1: a minimal .NET process that exposes normalized LibreHardwareMonitor telemetry through the versioned sidecar protocol.

The bridge must remain read-only initially and must not run inside the privileged broker.
