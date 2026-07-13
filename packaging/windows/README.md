# Windows packaging

The Windows package will install the gateway, broker, watchdog, CLI, provider sidecars, schemas, and baseline policies. Third-party applications remain user-installed during the first prototype.

Packaging must preserve target-specific binaries under `dist/<target-triple>/` and provide a complete uninstall rollback test.
