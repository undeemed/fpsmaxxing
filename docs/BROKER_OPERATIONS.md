# Broker operations and deployment

This page covers running `fpsmaxxing-broker`: where it puts its socket, journal, and lock, and the ownership rules it holds every one of those paths to.
For why the boundary is shaped this way, read [Architecture](ARCHITECTURE.md); for the trust boundaries it defends, read the [threat model](threat-model/README.md).

The `fpsmaxxing-broker` binary is the trusted side of the local IPC boundary.
It owns the control plane and serves capability discovery and the bounded provider lifecycle to authenticated local peers over a Unix domain socket.
Only the Unix domain socket transport is implemented, and the Windows named-pipe transport is not yet available, so the binary refuses to run there.
For what currently drives that boundary, see [Privileged broker](../README.md#privileged-broker) in the project README.

## Running the broker

Run it with no arguments; it creates and vets its own private directory for the socket and the journal.

```bash
cargo run -p fpsmaxxing-broker
cargo run -p fpsmaxxing-broker -- --help
```

An explicit path is never created for you, and the directory holding it must already be owned by the broker or root and closed to every other user (mode `0700`), so create it first:

```bash
mkdir -p "$HOME/.local/state/fpsmaxxing" && chmod 700 "$HOME/.local/state/fpsmaxxing"
cargo run -p fpsmaxxing-broker -- \
  --socket "$HOME/.local/state/fpsmaxxing/broker.sock" \
  --journal "$HOME/.local/state/fpsmaxxing/journal.sqlite"
```

## The private directory

The private directory is `$XDG_RUNTIME_DIR/fpsmaxxing`, or `/run/fpsmaxxing` when `XDG_RUNTIME_DIR` is unset, is not absolute, or the broker runs as root.
The broker creates it mode `0700` whether or not an override moved the socket and the journal out of it, because the single-instance lock lives there, and refuses to start unless it and every directory above it are owned by the broker or root and are not writable by anyone else.
It still establishes that directory even when both paths are given, so it also needs to be able to create `$XDG_RUNTIME_DIR/fpsmaxxing` - or `/run/fpsmaxxing`, when that variable is unset - on every start.

Do not put your own directory at `/run/fpsmaxxing`.
That is the privileged broker's own private directory, and it is the one directory held to exact ownership: root ownership satisfies an explicit `--socket` or `--journal` parent, but a broker accepts its private directory only when it owns that itself.
Creating `/run/fpsmaxxing` as your user therefore leaves a later root broker refusing to start until it is chowned to root or removed.
A root broker creates and vets it on its own.

A systemd unit needs both `RuntimeDirectory=fpsmaxxing` and `RuntimeDirectoryMode=0700`: `RuntimeDirectoryMode` defaults to `0755`, and the broker validates an existing private directory rather than correcting its mode, so a unit that omits the mode is refused on every start.

## Socket and journal paths

| Setting       | Flag               | Environment variable             | Default                        |
| ------------- | ------------------ | -------------------------------- | ------------------------------ |
| IPC socket    | `--socket <path>`  | `FPSMAXXING_BROKER_SOCKET`       | `<private dir>/broker.sock`    |
| Audit journal | `--journal <path>` | `FPSMAXXING_BROKER_JOURNAL_PATH` | `<private dir>/journal.sqlite` |

A flag wins over its environment variable, and both are broker-specific so nothing the gateway, the CLI, or the watchdog exports can move the privileged journal.

A path from a flag or an environment variable is held to the same bar as the default: it must be absolute, the directory holding it must exist, and the whole chain above it is vetted, so an override cannot place a privileged socket or audit journal somewhere another user can reach it.
Give the socket and the journal a directory of their own at mode `0700`, owned by the broker or root - the default private directory already is one.

That directory is held higher than the ancestors above it, in two ways.
The sticky bit does not excuse group or world write there: sticky stops another user removing the broker's socket or journal, but not creating either one first and keeping ownership of it, so a shared root like `/tmp` is refused.
Nor is group or world traversal excused: the socket's own mode cannot be pinned, so a merely traversable directory like `/run` would put every local user in front of it, and it is refused too.

The journal file itself is created mode `0600`, and SQLite's rollback journal and write-ahead log inherit that.

## Single-instance lock

Only one broker may run per user: it takes an exclusive lock on `<private dir>/broker.lock` before the journal is opened and before the socket is bound, so a second broker exits non-zero without having touched either.
That lock is not derived from `--socket` or `--journal`, so neither of those, nor the environment variables behind them, buys a second instance - the knobs two brokers would drive belong to the machine, not to the paths they were handed.
`XDG_RUNTIME_DIR` does move it, because it moves the private directory it sits in.
A root broker ignores that variable, so the privileged broker always locks `/run/fpsmaxxing/broker.lock` and one instance is guaranteed; an unprivileged user who runs two brokers under two different values for it gets two locks and two brokers, which is a dev-path concession rather than a boundary, since a same-uid caller is already admitted by the ACL.
The kernel releases the lock when the process ends, crash included, so a restart needs no cleanup - it rebinds over the socket file the previous run left behind.
