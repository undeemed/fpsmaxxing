//! Privileged provider broker process.
//!
//! Owns the control plane and serves capability discovery and bounded provider
//! lifecycles to authenticated local peers over a Unix domain socket. No raw
//! shell, Registry path, or hardware primitive crosses this boundary.
//!
//! Only the Unix domain socket transport is implemented; the Windows named-pipe
//! transport is deliberately out of scope, so the binary refuses to run there.
//!
//! The broker always establishes one owner-only private directory of its own,
//! under `$XDG_RUNTIME_DIR` (or `/run`). Unless `--socket`/`--journal` or their
//! environment overrides (`FPSMAXXING_BROKER_SOCKET` and
//! `FPSMAXXING_BROKER_JOURNAL_PATH`, both broker-only) name a path, the socket
//! and the journal live there rather than beside the inherited working
//! directory. Wherever a path came from, it is held to the same bar before it
//! is used: absolute, directly inside a directory no other user can reach,
//! under an ancestor chain no other user can write.
//!
//! Only one broker may run per uid. An exclusive advisory lock on a fixed file
//! in that private directory is taken before the journal is opened and before
//! the socket is bound, so a second process refuses to start rather than
//! driving the same knobs through an ownership ledger of its own. That is
//! unconditional for a root broker, whose private directory is always
//! `/run/fpsmaxxing`; an unprivileged one locks wherever the inherited
//! `XDG_RUNTIME_DIR` puts that directory, so the guard is best-effort on the
//! dev path it serves.
//!
//! The process exits non-zero on any fatal condition, including the loss of the
//! control-plane worker thread; run it under a supervisor that restarts it.

#[cfg(unix)]
#[tokio::main]
async fn main() {
    if let Err(error) = unix::run().await {
        eprintln!("fpsmaxxing-broker: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "fpsmaxxing-broker: only the Unix domain socket transport is implemented; the Windows named-pipe transport is not yet available"
    );
    std::process::exit(1);
}

#[cfg(unix)]
mod unix {
    use std::env;
    use std::error::Error;
    use std::ffi::{OsStr, OsString};
    use std::fs::{DirBuilder, File, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use fpsmaxxing_broker::{BrokerService, OwnershipLedger, serve, spawn_service};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport};
    use fpsmaxxing_mock_provider::MockProvider;
    use rustix::fs::{FlockOperation, flock};
    use rustix::io::Errno;
    use thiserror::Error;

    /// Value-taking flags this binary accepts, in [`USAGE`] order.
    const FLAGS: [&str; 2] = ["--socket", "--journal"];

    /// Arguments that ask for [`USAGE`] instead of a run.
    ///
    /// Recognized by [`parse_args`] only where a flag is expected, never as the
    /// value of one.
    const HELP_FLAGS: [&str; 2] = ["--help", "-h"];

    /// What `--help` prints.
    const USAGE: &str = "\
Usage: fpsmaxxing-broker [--socket <path>] [--journal <path>]

  --socket <path>   Unix domain socket to listen on
                    (environment: FPSMAXXING_BROKER_SOCKET)
  --journal <path>  SQLite audit journal to write
                    (environment: FPSMAXXING_BROKER_JOURNAL_PATH)
  -h, --help        Print this message and exit

A flag wins over its environment variable. Unset, each falls back into the
broker's own owner-only directory under $XDG_RUNTIME_DIR (or /run when it is
unset, is not absolute, or the broker runs as root); that directory is
established either way, because the lock that admits one broker per uid lives
in it. Every path must be absolute and sit in an existing directory owned by
the broker or root that no other user can reach (mode 0700), itself under a
chain of directories owned by the broker or root that no other user can write,
or the broker refuses to start.";

    /// Environment override for `--socket`.
    const SOCKET_ENV: &str = "FPSMAXXING_BROKER_SOCKET";

    /// Environment override for `--journal`.
    ///
    /// Deliberately distinct from the `FPSMAXXING_JOURNAL_PATH` the unprivileged
    /// gateway and CLI read. Sharing that variable would let an operator who
    /// exported it for the CLI silently move the privileged broker's audit
    /// journal out of its owner-only directory and into a file the gateway is
    /// writing concurrently.
    const JOURNAL_ENV: &str = "FPSMAXXING_BROKER_JOURNAL_PATH";

    /// Directory the broker keeps its socket and journal in by default.
    const PRIVATE_DIR_NAME: &str = "fpsmaxxing";

    /// Where [`PRIVATE_DIR_NAME`] lives when `XDG_RUNTIME_DIR` is not usable.
    const FALLBACK_RUNTIME_BASE: &str = "/run";

    /// The only mode the broker's private directory may have: owner access only.
    const PRIVATE_DIR_MODE: u32 = 0o700;

    /// Bits that make a directory writable by group or world.
    const OTHER_WRITE_BITS: u32 = 0o022;

    /// Bits that give any user but the owner access to a directory.
    const OTHER_ACCESS_BITS: u32 = 0o077;

    /// The sticky bit: only an entry's owner may rename or remove it.
    const STICKY_BIT: u32 = 0o1000;

    /// The superuser, trusted to own any ancestor of the private directory.
    const ROOT_UID: u32 = 0;

    /// Socket file name inside the broker's private directory.
    const DEFAULT_SOCKET_NAME: &str = "broker.sock";

    /// Journal file name inside the broker's private directory.
    const DEFAULT_JOURNAL_NAME: &str = "journal.sqlite";

    /// The only mode the audit journal may have: owner access only.
    const JOURNAL_FILE_MODE: u32 = 0o600;

    /// Suffixes `SQLite` appends to a database path for its side files.
    const JOURNAL_SIDE_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

    /// Single-instance lock file name inside the broker's private directory.
    ///
    /// Fixed rather than derived from the socket or the journal: the knobs two
    /// brokers would fight over are the machine's, not a path's, so an instance
    /// is scoped to the uid that runs it however it was pointed at its files.
    const LOCK_FILE_NAME: &str = "broker.lock";

    /// Why the command line was refused.
    ///
    /// A privileged daemon must not silently relocate its socket or journal
    /// because an argument was mistyped, so every parse failure is fatal.
    #[derive(Debug, Error)]
    pub enum ArgError {
        /// A recognized flag was given without a value.
        #[error("{0} requires a value")]
        MissingValue(String),
        /// A recognized flag was given more than once.
        #[error("{0} was given more than once")]
        Repeated(String),
        /// An argument is neither one of [`FLAGS`] nor one of [`HELP_FLAGS`].
        #[error("unrecognized argument {argument}; expected one of {expected}", expected = FLAGS.join(", "))]
        Unrecognized {
            /// The rejected argument, verbatim.
            argument: String,
        },
        /// An argument is not valid UTF-8.
        ///
        /// A path variable is read as a raw `OsString`, but the command line is
        /// matched against flag names, so a byte sequence that is not UTF-8 is
        /// reported as the typed failure it is rather than aborting the process
        /// mid-parse the way `env::args` would.
        #[error("argument {argument} is not valid UTF-8")]
        NotUnicode {
            /// The rejected argument, with each invalid sequence replaced.
            argument: String,
        },
    }

    /// Socket and journal locations resolved from the command line.
    #[derive(Debug, Default, Eq, PartialEq)]
    pub struct Options {
        /// Value of `--socket`, when supplied.
        pub socket: Option<String>,
        /// Value of `--journal`, when supplied.
        pub journal: Option<String>,
    }

    /// What a parsed command line asks the binary to do.
    #[derive(Debug, Eq, PartialEq)]
    pub enum Invocation {
        /// Serve with these locations.
        Run(Options),
        /// Print [`USAGE`] and exit successfully.
        Help,
    }

    pub async fn run() -> Result<(), Box<dyn Error>> {
        let options = match parse_args(env::args_os().skip(1))? {
            Invocation::Help => {
                println!("{USAGE}");
                return Ok(());
            }
            Invocation::Run(options) => options,
        };
        let authorizer = SameUidAuthorizer::for_current_process();
        let broker_uid = authorizer.expected_uid();
        let Paths {
            socket: socket_path,
            journal: journal_path,
            lock: lock_path,
        } = resolve_paths(options, broker_uid)?;
        let _single_instance = lock_single_instance(&lock_path)?;

        let ledger = Arc::new(OwnershipLedger::new());
        let broker = spawn_service(move || {
            let provider = Box::new(MockProvider::new(0));
            restrict_journal(&journal_path)?;
            let plane = ControlPlane::open(provider, &journal_path).map_err(io::Error::other)?;
            Ok(BrokerService::new(plane, ledger))
        })
        .await?;

        let transport = UnixSocketTransport::bind(&socket_path)?;
        let authorizer: Arc<dyn PeerAuthorizer> = Arc::new(authorizer);
        println!(
            "fpsmaxxing-broker: listening on {} (same-uid ACL, uid {broker_uid})",
            socket_path.display()
        );
        serve(transport, broker, authorizer).await?;
        Ok(())
    }

    /// Every path a running broker owns.
    #[derive(Debug, Eq, PartialEq)]
    pub struct Paths {
        /// Where the IPC endpoint is bound.
        pub socket: PathBuf,
        /// Where the audit journal is written.
        pub journal: PathBuf,
        /// The file [`lock_single_instance`] locks.
        pub lock: PathBuf,
    }

    /// Resolves the socket, journal, and lock locations, in override order.
    ///
    /// An explicit flag wins over the matching environment variable. Anything
    /// left unset falls back into the broker's private directory rather than
    /// beside the inherited working directory, so a privileged daemon never
    /// places its IPC endpoint or its durable audit journal somewhere it does
    /// not own.
    ///
    /// The environment is read with [`env::var_os`] rather than `env::var`, so a
    /// path that is not UTF-8 relocates the socket or journal as configured
    /// instead of being silently dropped back to the default.
    fn resolve_paths(options: Options, broker_uid: u32) -> io::Result<Paths> {
        let base = runtime_base(env::var_os("XDG_RUNTIME_DIR").as_deref(), broker_uid);
        resolve_paths_from(options, broker_uid, &base, |name| env::var_os(name))
    }

    /// [`resolve_paths`] against an arbitrary runtime base and environment.
    ///
    /// Only [`SOCKET_ENV`] and [`JOURNAL_ENV`] are ever consulted; the broker
    /// shares no path variable with the unprivileged gateway or CLI.
    ///
    /// The private directory under `base` is created and vetted whatever the
    /// overrides say, because the single-instance lock lives in it and must not
    /// move with them: a lock keyed to the journal would let two brokers pointed
    /// at different journals both start and then share one socket, since only
    /// the path that was left unset falls back to the default.
    ///
    /// Every resolved path is put through [`vet_resolved_path`], whatever named
    /// it. An environment variable is inherited from whoever started the broker,
    /// so honoring one verbatim would hand that caller the choice of where a
    /// root-owned socket and audit journal are created - exactly what
    /// [`runtime_base`] refuses them for `XDG_RUNTIME_DIR`.
    fn resolve_paths_from<F>(
        options: Options,
        broker_uid: u32,
        base: &Path,
        lookup: F,
    ) -> io::Result<Paths>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let socket = options
            .socket
            .map(OsString::from)
            .or_else(|| lookup(SOCKET_ENV));
        let journal = options
            .journal
            .map(OsString::from)
            .or_else(|| lookup(JOURNAL_ENV));
        let directory = private_directory_in(base, broker_uid)?;
        let paths = Paths {
            socket: socket.map_or_else(|| directory.join(DEFAULT_SOCKET_NAME), PathBuf::from),
            journal: journal.map_or_else(|| directory.join(DEFAULT_JOURNAL_NAME), PathBuf::from),
            lock: directory.join(LOCK_FILE_NAME),
        };
        vet_resolved_path(&paths.socket, broker_uid)?;
        vet_resolved_path(&paths.journal, broker_uid)?;
        Ok(paths)
    }

    /// Refuses a resolved socket or journal path the broker must not use.
    ///
    /// A relative path would resolve against the inherited working directory,
    /// which is as much the caller's choice as the variable that named it. A
    /// parent another user may write is what makes the socket path raceable at
    /// bind time and the journal readable, so the whole chain above the file is
    /// vetted - the same vet the default directory gets.
    ///
    /// The directory that directly holds the entry is held to a stricter bar
    /// than the ancestors above it: owner access only, sticky or not. Sticky
    /// stops another user renaming or removing the broker's socket or journal,
    /// but not creating that entry first in a shared directory like `/tmp` and
    /// keeping ownership of the file a privileged broker then writes every
    /// `apply-intent` record into; and a merely traversable directory like
    /// `/run` puts every local user in front of a socket whose own mode cannot
    /// be pinned. Higher up, neither is the threat - swapping a vetted
    /// directory is - and sticky does prevent that.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] for a path that is not
    /// absolute or names no parent directory, or whatever [`vet_directory`]
    /// refuses the parent or an ancestor with.
    fn vet_resolved_path(path: &Path, broker_uid: u32) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|_| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} must be an absolute path to an entry inside a directory",
                        path.display()
                    ),
                )
            })?;
        vet_directory(parent, broker_uid, Bar::OwnerOnly)?;
        match parent.parent() {
            Some(above) => vet_ancestors(above, broker_uid),
            None => Ok(()),
        }
    }

    /// Takes the exclusive lock that makes this process the only broker.
    ///
    /// Single-owner-per-knob holds inside one process because one ownership
    /// ledger arbitrates it, and two brokers would hold one each. Nothing about
    /// binding the socket closes that across processes: an existing entry
    /// cannot be told apart from a live endpoint without a probe, and a probe
    /// is not a lock - two brokers can both find the path stale, and the second
    /// one's unlink then strands the first on an unlinked inode, still serving
    /// its connected clients and still driving the same knobs.
    ///
    /// An advisory lock has no such window. It is taken here, before the
    /// journal is created or opened and before the socket is bound, so a second
    /// broker touches neither. The kernel drops it when the last descriptor for
    /// it closes, so a crashed broker leaves nothing to clean up - which is why
    /// the returned file must be held for as long as the broker serves.
    ///
    /// The lock file is [`LOCK_FILE_NAME`] in the broker's private directory,
    /// which [`private_directory_in`] has already held to owner-only access, so
    /// no other user can create it first or take it. Its location is fixed
    /// rather than derived from the socket or the journal, so an instance is
    /// scoped to the uid that runs it: what two brokers contend for is the
    /// machine's knobs, which no path override makes separate. The directory
    /// holding it still follows `XDG_RUNTIME_DIR` for an unprivileged broker,
    /// so a single instance is guaranteed only for a root broker, which
    /// [`runtime_base`] refuses that variable for.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::AddrInUse`] when another broker holds the lock,
    /// or an error naming the lock file when it cannot be created or locked.
    fn lock_single_instance(path: &Path) -> io::Result<File> {
        let file = create_owner_only(path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(file),
            Err(errno) if errno == Errno::WOULDBLOCK => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "{} is held by a running broker; only one may own this machine's knobs",
                    path.display()
                ),
            )),
            Err(errno) => Err(named(path, "locked", &io::Error::from(errno))),
        }
    }

    /// Restricts the audit journal to its owner before the journal is opened.
    ///
    /// `SQLite` creates a database with mode `0666` masked by the inherited
    /// umask, so an operator-supplied path outside the broker's own `0700`
    /// directory would otherwise hold every `apply-intent` record - the full
    /// change request - in a world-readable file. Creating the database
    /// owner-only first closes that, and closes it for the rollback journal and
    /// write-ahead log too: `SQLite` copies the database file's mode onto the
    /// side files it creates beside it. A side file left behind by an earlier
    /// run is restricted directly, since nothing will recreate it.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be created or if it or an existing
    /// side file cannot be restricted.
    fn restrict_journal(path: &Path) -> io::Result<()> {
        drop(create_owner_only(path)?);
        for suffix in JOURNAL_SIDE_SUFFIXES {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            match restrict_to_owner(Path::new(&side)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Opens `path` at [`JOURNAL_FILE_MODE`], creating it when it is absent.
    ///
    /// The requested mode only applies to a file this call creates, and the
    /// inherited umask strips bits from it even then, so the mode is reapplied
    /// afterwards. Both files the broker owns outright - the audit journal and
    /// the single-instance lock - are created through here, so neither can be
    /// left readable by another user.
    ///
    /// # Errors
    ///
    /// Returns an error naming `path` if it cannot be opened or restricted.
    fn create_owner_only(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(JOURNAL_FILE_MODE)
            .open(path)
            .map_err(|error| named(path, "opened", &error))?;
        restrict_to_owner(path)?;
        Ok(file)
    }

    /// Sets `path` to [`JOURNAL_FILE_MODE`], naming it on failure.
    fn restrict_to_owner(path: &Path) -> io::Result<()> {
        std::fs::set_permissions(path, Permissions::from_mode(JOURNAL_FILE_MODE))
            .map_err(|error| named(path, "restricted", &error))
    }

    /// Names `path` in `error`, which `std`'s io errors never carry themselves.
    ///
    /// The broker takes three configurable paths, so a bare `Permission denied`
    /// leaves an operator no way to tell which of them the start-up failed on.
    fn named(path: &Path, action: &str, error: &io::Error) -> io::Error {
        io::Error::new(
            error.kind(),
            format!("{} cannot be {action}: {error}", path.display()),
        )
    }

    /// Chooses the base directory [`PRIVATE_DIR_NAME`] is created under.
    ///
    /// `XDG_RUNTIME_DIR` is a session variable the broker inherits from whoever
    /// started it, so it is honored only when it names an absolute path and only
    /// for an unprivileged broker. A broker running as root ignores it outright:
    /// root has no per-session runtime directory, and following an inherited one
    /// would let the caller choose where a root-owned socket and audit journal
    /// are created.
    fn runtime_base(xdg_runtime_dir: Option<&OsStr>, broker_uid: u32) -> PathBuf {
        if broker_uid == ROOT_UID {
            return PathBuf::from(FALLBACK_RUNTIME_BASE);
        }
        xdg_runtime_dir
            .map(PathBuf::from)
            .filter(|base| base.is_absolute())
            .unwrap_or_else(|| PathBuf::from(FALLBACK_RUNTIME_BASE))
    }

    /// Creates and vets [`PRIVATE_DIR_NAME`] under `base`.
    ///
    /// `base` and every ancestor above it are vetted first, because a parent
    /// another user may write is what makes the socket path raceable at bind
    /// time and the journal readable: such a user could swap a vetted directory
    /// for a symlink before the bind and steer a privileged broker's socket and
    /// journal to a path of their choosing.
    ///
    /// A directory the broker created is vetted too: the inherited umask can
    /// strip bits from the requested mode - which is why the mode is reapplied
    /// after a successful create - and the entry may already have been something
    /// else.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, or if it is not a
    /// directory owned by `broker_uid` with mode [`PRIVATE_DIR_MODE`], or if
    /// [`vet_ancestors`] refuses the path it sits in.
    fn private_directory_in(base: &Path, broker_uid: u32) -> io::Result<PathBuf> {
        vet_ancestors(base, broker_uid)?;
        let directory = base.join(PRIVATE_DIR_NAME);
        match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(&directory) {
            Ok(()) => {
                std::fs::set_permissions(&directory, Permissions::from_mode(PRIVATE_DIR_MODE))
                    .map_err(|error| named(&directory, "restricted", &error))?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(named(&directory, "created", &error)),
        }
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| named(&directory, "inspected", &error))?;
        if metadata.is_dir()
            && metadata.uid() == broker_uid
            && metadata.mode() & 0o777 == PRIVATE_DIR_MODE
        {
            Ok(directory)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a directory owned by uid {broker_uid} with mode {PRIVATE_DIR_MODE:o}",
                    directory.display()
                ),
            ))
        }
    }

    /// How much of a directory's mode the broker insists on.
    #[derive(Clone, Copy)]
    enum Bar {
        /// The bar for a directory that directly holds the socket or the
        /// journal: no other user may read, write, or traverse it. Traversal is
        /// what puts a peer in front of the socket, and the socket's own mode
        /// cannot be pinned, so the directory is the only place to deny it.
        OwnerOnly,
        /// The bar for an ancestor: no other user may write it, unless it is
        /// sticky. The threat an ancestor carries is the swap of the directory
        /// below it for one the broker does not own, and sticky - only an
        /// entry's owner may rename or remove it - is exactly what prevents
        /// that. This is how `/tmp` and similar shared roots are protected.
        NoForeignWrite,
    }

    impl Bar {
        /// Whether `mode` gives some other user access this bar denies.
        fn refuses(self, mode: u32) -> bool {
            match self {
                Self::OwnerOnly => mode & OTHER_ACCESS_BITS != 0,
                Self::NoForeignWrite => mode & OTHER_WRITE_BITS != 0 && mode & STICKY_BIT == 0,
            }
        }

        /// How the refusal reads to an operator.
        fn requirement(self) -> &'static str {
            match self {
                Self::OwnerOnly => "no other user can reach",
                Self::NoForeignWrite => "no other user can write",
            }
        }
    }

    /// Refuses a directory another user could tamper with or reach through.
    ///
    /// It must be a real directory - not a symlink - owned by the broker or by
    /// root, and closed to other users to whatever degree `bar` demands.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] if the directory fails the
    /// bar, or the underlying error - named with the directory, since a path
    /// the broker will not create is the likeliest reason it cannot be
    /// inspected - if it cannot be inspected at all.
    fn vet_directory(directory: &Path, broker_uid: u32, bar: Bar) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(directory)
            .map_err(|error| named(directory, "inspected", &error))?;
        let owned_by_trusted_uid = metadata.uid() == broker_uid || metadata.uid() == ROOT_UID;
        if !metadata.is_dir() || !owned_by_trusted_uid || bar.refuses(metadata.mode()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a directory owned by uid {broker_uid} or root that {requirement} it",
                    directory.display(),
                    requirement = bar.requirement()
                ),
            ));
        }
        Ok(())
    }

    /// Refuses a base whose own path another user could tamper with.
    ///
    /// Every component from `base` up to `/` goes through [`vet_directory`] at
    /// [`Bar::NoForeignWrite`].
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] for the first component that
    /// fails, or the underlying error if a component cannot be inspected.
    fn vet_ancestors(base: &Path, broker_uid: u32) -> io::Result<()> {
        for ancestor in base.ancestors() {
            vet_directory(ancestor, broker_uid, Bar::NoForeignWrite)?;
        }
        Ok(())
    }

    /// Whether an argument standing in value position is really a flag.
    ///
    /// Every value-taking flag is long, and [`HELP_FLAGS`] adds the one short
    /// flag, so a value matching either is a mistyped command line, not a path.
    fn looks_like_flag(value: &str) -> bool {
        value.starts_with("--") || HELP_FLAGS.contains(&value)
    }

    /// Parses `--flag value` and `--flag=value` pairs, rejecting anything else.
    ///
    /// A help flag is recognized only where a flag is expected. In value
    /// position it is refused like any other flag-shaped value, so the mistyped
    /// `--socket --help` stays fatal instead of exiting successfully - which a
    /// supervisor would read as a clean stop of a broker that never came up.
    ///
    /// In the separated form the value may not itself look like a flag, so
    /// `--socket --journal /var/j.sqlite` is a mistyped command line rather than
    /// a socket literally named `--journal`. Use `--socket=--journal` to mean it.
    ///
    /// Arguments arrive as `OsString`, so a byte sequence that is not UTF-8
    /// becomes [`ArgError::NotUnicode`] rather than the mid-iteration panic
    /// `env::args` would raise.
    ///
    /// # Errors
    ///
    /// Returns [`ArgError`] for an argument that is not UTF-8 or is
    /// unrecognized, a repeated flag, or a flag whose value is missing, empty,
    /// or another flag.
    pub fn parse_args<I>(arguments: I) -> Result<Invocation, ArgError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut options = Options::default();
        let mut arguments = arguments.into_iter().map(|argument| {
            argument
                .into_string()
                .map_err(|argument| ArgError::NotUnicode {
                    argument: argument.to_string_lossy().into_owned(),
                })
        });
        while let Some(argument) = arguments.next() {
            let argument = argument?;
            if HELP_FLAGS.contains(&argument.as_str()) {
                return Ok(Invocation::Help);
            }
            let (flag, inline) = match argument.split_once('=') {
                Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
                None => (argument.clone(), None),
            };
            let slot = match flag.as_str() {
                "--socket" => &mut options.socket,
                "--journal" => &mut options.journal,
                _ => return Err(ArgError::Unrecognized { argument }),
            };
            if slot.is_some() {
                return Err(ArgError::Repeated(flag));
            }
            let value = match inline {
                Some(value) => value,
                None => arguments
                    .next()
                    .transpose()?
                    .filter(|value| !looks_like_flag(value))
                    .ok_or_else(|| ArgError::MissingValue(flag.clone()))?,
            };
            if value.is_empty() {
                return Err(ArgError::MissingValue(flag));
            }
            *slot = Some(value);
        }
        Ok(Invocation::Run(options))
    }

    #[cfg(test)]
    mod tests {
        use std::cell::RefCell;
        use std::collections::BTreeSet;
        use std::ffi::{OsStr, OsString};
        use std::io;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::Path;

        use fpsmaxxing_control_plane::ControlPlane;
        use fpsmaxxing_mock_provider::MockProvider;

        use super::{
            ArgError, FALLBACK_RUNTIME_BASE, HELP_FLAGS, Invocation, JOURNAL_ENV,
            JOURNAL_FILE_MODE, LOCK_FILE_NAME, Options, PRIVATE_DIR_MODE, PRIVATE_DIR_NAME,
            ROOT_UID, SOCKET_ENV, lock_single_instance, parse_args, private_directory_in,
            resolve_paths_from, restrict_journal, runtime_base,
        };

        /// The variable the unprivileged gateway and CLI use for their journal.
        const GATEWAY_JOURNAL_ENV: &str = "FPSMAXXING_JOURNAL_PATH";

        /// An unprivileged uid, so `XDG_RUNTIME_DIR` is eligible at all.
        const SESSION_UID: u32 = 1000;

        fn parse(arguments: &[&str]) -> Result<Invocation, ArgError> {
            parse_args(arguments.iter().map(OsString::from))
        }

        /// The [`Options`] a command line that asks for a run resolves to.
        fn run_options(arguments: &[&str]) -> Options {
            match parse(arguments).expect("the command line should parse") {
                Invocation::Run(options) => options,
                Invocation::Help => panic!("{arguments:?} should not ask for usage"),
            }
        }

        /// A lookup that answers every name with a vettable path under `base`.
        ///
        /// It records which names were asked for, so a test can prove the broker
        /// never reaches for a variable that is not its own.
        fn recording_lookup<'a>(
            base: &'a Path,
            seen: &'a RefCell<BTreeSet<String>>,
        ) -> impl Fn(&str) -> Option<OsString> + 'a {
            move |name| {
                seen.borrow_mut().insert(name.to_owned());
                Some(base.join(name).into_os_string())
            }
        }

        /// A lookup that answers only [`SOCKET_ENV`], with `value`.
        fn socket_env_lookup(value: &Path) -> impl Fn(&str) -> Option<OsString> + '_ {
            move |name| (name == SOCKET_ENV).then(|| value.as_os_str().to_owned())
        }

        /// The uid the test process creates files as, read from one it created.
        fn own_uid(created: &Path) -> u32 {
            std::fs::symlink_metadata(created)
                .expect("a just-created path should stat")
                .uid()
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("mode should apply");
        }

        /// A temporary directory no other user can reach.
        ///
        /// `tempfile` honors the inherited umask, which typically leaves the
        /// directory traversable - the one shape a directory that directly
        /// holds the socket or the journal may not have.
        fn owner_only_tempdir() -> tempfile::TempDir {
            let directory = tempfile::tempdir().expect("temporary directory should exist");
            chmod(directory.path(), PRIVATE_DIR_MODE);
            directory
        }

        /// A path as the `String` a command-line flag would carry.
        fn path_string(path: &Path) -> String {
            path.to_str()
                .expect("a temporary path should be UTF-8")
                .to_owned()
        }

        #[test]
        fn both_flag_forms_are_accepted() {
            assert_eq!(
                run_options(&["--socket", "/run/b.sock", "--journal=/var/j.sqlite"]),
                Options {
                    socket: Some("/run/b.sock".to_owned()),
                    journal: Some("/var/j.sqlite".to_owned()),
                }
            );
            assert_eq!(run_options(&[]), Options::default());
        }

        #[test]
        fn a_flag_shaped_value_needs_the_inline_form() {
            assert!(matches!(
                parse(&["--socket", "--journal", "/var/j.sqlite"])
                    .expect_err("a swallowed flag must not become the socket path"),
                ArgError::MissingValue(_)
            ));
            let options = run_options(&["--socket=--journal"]);
            assert_eq!(options.socket.as_deref(), Some("--journal"));
        }

        #[test]
        fn a_mistyped_command_line_is_fatal() {
            assert!(matches!(
                parse(&["--socket"])
                    .expect_err("a trailing flag must not fall back to the default"),
                ArgError::MissingValue(_)
            ));
            assert!(matches!(
                parse(&["--socket="]).expect_err("an empty value must be refused"),
                ArgError::MissingValue(_)
            ));
            assert!(matches!(
                parse(&["--sockett", "/run/b.sock"]).expect_err("a typo must be refused"),
                ArgError::Unrecognized { .. }
            ));
            assert!(matches!(
                parse(&["/run/b.sock"]).expect_err("a positional argument must be refused"),
                ArgError::Unrecognized { .. }
            ));
            assert!(matches!(
                parse(&["--socket", "/a", "--socket", "/b"])
                    .expect_err("a repeated flag must be refused"),
                ArgError::Repeated(_)
            ));
        }

        #[test]
        fn a_non_utf8_argument_is_a_typed_error_not_a_panic() {
            let invalid = OsString::from_vec(vec![b'/', 0xff, b'x']);
            for arguments in [
                vec![invalid.clone()],
                vec![OsString::from("--socket"), invalid.clone()],
            ] {
                assert!(
                    matches!(
                        parse_args(arguments).expect_err("a non-UTF-8 argument must be refused"),
                        ArgError::NotUnicode { .. }
                    ),
                    "a privileged daemon must fail with a named argument, not a panic"
                );
            }
        }

        #[test]
        fn the_journal_never_comes_from_the_gateway_environment() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let seen = RefCell::new(BTreeSet::new());
            let paths = resolve_paths_from(
                Options::default(),
                uid,
                base.path(),
                recording_lookup(base.path(), &seen),
            )
            .expect("both paths come from the environment");

            assert_eq!(paths.socket, base.path().join(SOCKET_ENV));
            assert_eq!(paths.journal, base.path().join(JOURNAL_ENV));
            assert_eq!(JOURNAL_ENV, "FPSMAXXING_BROKER_JOURNAL_PATH");
            assert!(
                !seen.borrow().contains(GATEWAY_JOURNAL_ENV),
                "the privileged broker must not read the gateway's journal variable"
            );
            assert_eq!(
                *seen.borrow(),
                [SOCKET_ENV.to_owned(), JOURNAL_ENV.to_owned()]
                    .into_iter()
                    .collect()
            );
        }

        #[test]
        fn explicit_flags_win_over_the_environment() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let seen = RefCell::new(BTreeSet::new());
            let options = Options {
                socket: Some(path_string(&base.path().join("b.sock"))),
                journal: Some(path_string(&base.path().join("j.sqlite"))),
            };
            let paths = resolve_paths_from(
                options,
                uid,
                base.path(),
                recording_lookup(base.path(), &seen),
            )
            .expect("explicit flags name both endpoints");
            assert_eq!(paths.socket, base.path().join("b.sock"));
            assert_eq!(paths.journal, base.path().join("j.sqlite"));
            assert!(
                seen.borrow().is_empty(),
                "a flag must not consult the environment at all"
            );
        }

        #[test]
        fn the_instance_lock_stays_in_the_private_directory_whatever_the_overrides() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let private = base.path().join(PRIVATE_DIR_NAME);

            // Two brokers pointed at different journals are still one instance:
            // the knobs they would both drive belong to the machine, not to a
            // path either of them was handed.
            let mut locks = Vec::new();
            for journal in ["a.sqlite", "b.sqlite"] {
                let options = Options {
                    journal: Some(path_string(&base.path().join(journal))),
                    ..Options::default()
                };
                let paths = resolve_paths_from(options, uid, base.path(), |_| None)
                    .expect("an owner-only base is sound");
                assert_eq!(
                    paths.socket,
                    private.join("broker.sock"),
                    "only the path left unset falls back, so both share one socket"
                );
                assert_eq!(paths.lock, private.join(LOCK_FILE_NAME));
                locks.push(paths.lock);
            }

            let _held = lock_single_instance(&locks[0]).expect("the first broker should be alone");
            let error = lock_single_instance(&locks[1])
                .expect_err("a second broker on this machine must be refused");
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        }

        #[test]
        fn the_private_directory_is_established_even_when_both_paths_are_overridden() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let options = Options {
                socket: Some(path_string(&base.path().join("b.sock"))),
                journal: Some(path_string(&base.path().join("j.sqlite"))),
            };
            let paths = resolve_paths_from(options, uid, base.path(), |_| None)
                .expect("an owner-only base is sound");

            let private = base.path().join(PRIVATE_DIR_NAME);
            assert_eq!(paths.lock, private.join(LOCK_FILE_NAME));
            let metadata =
                std::fs::symlink_metadata(&private).expect("the private directory should stat");
            assert!(metadata.is_dir());
            assert_eq!(
                metadata.mode() & 0o777,
                PRIVATE_DIR_MODE,
                "the lock must live somewhere no other user can take it first"
            );
        }

        #[test]
        fn a_relative_path_from_the_environment_is_refused() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let options = Options {
                journal: Some(path_string(&base.path().join("j.sqlite"))),
                ..Options::default()
            };
            let error = resolve_paths_from(
                options,
                uid,
                base.path(),
                socket_env_lookup(Path::new("broker.sock")),
            )
            .expect_err("a relative override would land beside the inherited cwd");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn a_path_from_the_environment_under_a_writable_parent_is_refused() {
            let outer = owner_only_tempdir();
            let uid = own_uid(outer.path());
            let reachable = outer.path().join("reachable");
            std::fs::create_dir(&reachable).expect("directory should create");
            chmod(&reachable, 0o777);

            let options = Options {
                journal: Some(path_string(&outer.path().join("j.sqlite"))),
                ..Options::default()
            };
            let error = resolve_paths_from(
                options,
                uid,
                outer.path(),
                socket_env_lookup(&reachable.join("broker.sock")),
            )
            .expect_err("an override under a world-writable parent must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

            // The same override is accepted once nobody else can write its parent.
            chmod(&reachable, PRIVATE_DIR_MODE);
            let options = Options {
                journal: Some(path_string(&outer.path().join("j.sqlite"))),
                ..Options::default()
            };
            let paths = resolve_paths_from(
                options,
                uid,
                outer.path(),
                socket_env_lookup(&reachable.join("broker.sock")),
            )
            .expect("an owner-only parent is sound");
            assert_eq!(paths.socket, reachable.join("broker.sock"));
        }

        #[test]
        fn a_sticky_directory_may_not_hold_the_socket_or_the_journal() {
            let outer = owner_only_tempdir();
            let uid = own_uid(outer.path());
            let shared = outer.path().join("shared");
            std::fs::create_dir(&shared).expect("directory should create");
            chmod(&shared, 0o1777);

            // Sticky stops another user removing the broker's entry, not
            // creating it first and keeping ownership of what the broker writes.
            let options = Options {
                socket: Some(path_string(&shared.join("broker.sock"))),
                journal: Some(path_string(&outer.path().join("j.sqlite"))),
            };
            let error = resolve_paths_from(options, uid, outer.path(), |_| None)
                .expect_err("a sticky world-writable parent must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

            let options = Options {
                socket: Some(path_string(&outer.path().join("b.sock"))),
                journal: Some(path_string(&shared.join("j.sqlite"))),
            };
            let error = resolve_paths_from(options, uid, outer.path(), |_| None)
                .expect_err("the journal is held to the same bar as the socket");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

            // Higher up, sticky is sound: it prevents the swap of the
            // owner-only directory that does hold them.
            let private = shared.join(PRIVATE_DIR_NAME);
            std::fs::create_dir(&private).expect("directory should create");
            chmod(&private, PRIVATE_DIR_MODE);
            let options = Options {
                socket: Some(path_string(&private.join("broker.sock"))),
                journal: Some(path_string(&private.join("j.sqlite"))),
            };
            let paths = resolve_paths_from(options, uid, shared.as_path(), |_| None)
                .expect("an owner-only directory under a sticky ancestor is sound");
            assert_eq!(paths.socket, private.join("broker.sock"));
            assert_eq!(paths.journal, private.join("j.sqlite"));
        }

        #[test]
        fn a_traversable_directory_may_not_hold_the_socket_or_the_journal() {
            let outer = owner_only_tempdir();
            let uid = own_uid(outer.path());
            // The shape of /run: nobody else may write it, but everybody may
            // traverse it, and the socket's own mode cannot be pinned.
            let traversable = outer.path().join("run");
            std::fs::create_dir(&traversable).expect("directory should create");
            chmod(&traversable, 0o755);

            for options in [
                Options {
                    socket: Some(path_string(&traversable.join("broker.sock"))),
                    journal: Some(path_string(&outer.path().join("j.sqlite"))),
                },
                Options {
                    socket: Some(path_string(&outer.path().join("b.sock"))),
                    journal: Some(path_string(&traversable.join("j.sqlite"))),
                },
            ] {
                let error = resolve_paths_from(options, uid, outer.path(), |_| None)
                    .expect_err("a world-traversable parent must be refused");
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }

            // Only the owner-only shape the broker's own default already has
            // is accepted, even though the ancestor above it stays traversable.
            let private = traversable.join(PRIVATE_DIR_NAME);
            std::fs::create_dir(&private).expect("directory should create");
            chmod(&private, PRIVATE_DIR_MODE);
            let options = Options {
                socket: Some(path_string(&private.join("broker.sock"))),
                journal: Some(path_string(&private.join("j.sqlite"))),
            };
            let paths = resolve_paths_from(options, uid, traversable.as_path(), |_| None)
                .expect("an owner-only directory under a traversable ancestor is sound");
            assert_eq!(paths.socket, private.join("broker.sock"));
            assert_eq!(paths.journal, private.join("j.sqlite"));
        }

        #[test]
        fn a_flag_path_is_vetted_the_same_way_as_the_environment() {
            let base = owner_only_tempdir();
            let uid = own_uid(base.path());
            let options = Options {
                socket: Some("broker.sock".to_owned()),
                journal: Some(path_string(&base.path().join("j.sqlite"))),
            };
            let error = resolve_paths_from(options, uid, base.path(), |_| None)
                .expect_err("a flag must not bypass the vet an override is held to");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn the_journal_is_owner_only_once_it_is_open() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let journal = base.path().join("journal.sqlite");
            restrict_journal(&journal).expect("the journal should be restricted");
            let plane = ControlPlane::open(Box::new(MockProvider::new(0)), &journal)
                .expect("control plane should open");
            drop(plane);

            let metadata = std::fs::symlink_metadata(&journal).expect("journal should stat");
            assert_eq!(
                metadata.mode() & 0o777,
                JOURNAL_FILE_MODE,
                "the privileged audit journal must not be readable by anyone else"
            );
        }

        #[test]
        fn an_existing_journal_and_its_side_files_are_restricted() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let journal = base.path().join("journal.sqlite");
            std::fs::write(&journal, b"").expect("journal should create");
            chmod(&journal, 0o644);
            let side = base.path().join("journal.sqlite-wal");
            std::fs::write(&side, b"").expect("side file should create");
            chmod(&side, 0o644);

            restrict_journal(&journal).expect("an existing journal should be restricted");
            for path in [&journal, &side] {
                let metadata = std::fs::symlink_metadata(path).expect("path should stat");
                assert_eq!(metadata.mode() & 0o777, JOURNAL_FILE_MODE);
            }
        }

        #[test]
        fn a_second_broker_cannot_take_the_instance_lock() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let lock = base.path().join(LOCK_FILE_NAME);
            let journal = base.path().join("journal.sqlite");

            let held = lock_single_instance(&lock).expect("the first broker should be alone");
            let error = lock_single_instance(&lock)
                .expect_err("a second broker must not run beside the first");
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            assert!(
                !journal.exists(),
                "a refused broker must not have touched the incumbent's journal"
            );

            // The kernel releases the lock with the last descriptor for it, so
            // a crashed broker leaves nothing for its restart to clean up.
            drop(held);
            lock_single_instance(&lock).expect("a released lock should be takeable again");
        }

        #[test]
        fn the_instance_lock_is_owner_only() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let lock = base.path().join(LOCK_FILE_NAME);
            let _held = lock_single_instance(&lock).expect("the lock should be taken");

            let metadata = std::fs::symlink_metadata(&lock).expect("the lock file should stat");
            assert_eq!(metadata.mode() & 0o777, JOURNAL_FILE_MODE);
        }

        #[test]
        fn a_help_flag_is_recognized_only_in_flag_position() {
            for flag in HELP_FLAGS {
                assert_eq!(
                    parse(&[flag]).expect("a help flag should parse"),
                    Invocation::Help,
                    "{flag} should ask for usage"
                );
                assert_eq!(
                    parse(&["--journal", "/var/j.sqlite", flag])
                        .expect("a help flag should parse anywhere a flag may stand"),
                    Invocation::Help
                );
                assert!(
                    matches!(parse(&["--socket", flag]), Err(ArgError::MissingValue(_))),
                    "{flag} in value position is a mistyped command line, not usage"
                );
            }
        }

        #[test]
        fn an_unusable_runtime_directory_falls_back_to_run() {
            let fallback = Path::new(FALLBACK_RUNTIME_BASE);
            assert_eq!(runtime_base(None, SESSION_UID), fallback);
            assert_eq!(runtime_base(Some(OsStr::new("")), SESSION_UID), fallback);
            assert_eq!(
                runtime_base(Some(OsStr::new("run/user/1000")), SESSION_UID),
                fallback,
                "a relative runtime directory would resolve against the inherited cwd"
            );
            assert_eq!(
                runtime_base(Some(OsStr::new("/run/user/1000")), SESSION_UID),
                Path::new("/run/user/1000")
            );
        }

        #[test]
        fn a_privileged_broker_ignores_the_inherited_runtime_directory() {
            assert_eq!(
                runtime_base(Some(OsStr::new("/tmp/attacker-owned")), ROOT_UID),
                Path::new(FALLBACK_RUNTIME_BASE),
                "root must not follow a runtime directory its caller chose"
            );
        }

        #[test]
        fn a_base_under_a_writable_ancestor_is_refused() {
            let outer = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(outer.path());
            let base = outer.path().join("base");
            std::fs::create_dir(&base).expect("base should create");
            chmod(&base, PRIVATE_DIR_MODE);

            // The base itself is sound; the directory holding it is not, so
            // another user could swap the base for a symlink after the vet.
            chmod(outer.path(), 0o777);
            let error = private_directory_in(&base, uid)
                .expect_err("a world-writable ancestor must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                !base.join(PRIVATE_DIR_NAME).exists(),
                "nothing may be created under a refused ancestor"
            );

            // Restoring the ancestor, and making it sticky instead, both pass.
            chmod(outer.path(), 0o755);
            private_directory_in(&base, uid).expect("a sound ancestor chain is accepted");
            chmod(outer.path(), 0o1777);
            private_directory_in(&base, uid).expect("a sticky ancestor cannot be swapped");
        }

        #[test]
        fn a_symlinked_base_is_refused() {
            let outer = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(outer.path());
            let target = outer.path().join("target");
            std::fs::create_dir(&target).expect("target should create");
            chmod(&target, PRIVATE_DIR_MODE);
            let base = outer.path().join("base");
            std::os::unix::fs::symlink(&target, &base).expect("symlink should create");

            let error =
                private_directory_in(&base, uid).expect_err("a symlinked base must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn a_missing_private_directory_is_created_owner_only() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let directory =
                private_directory_in(base.path(), uid).expect("the directory should be created");
            assert_eq!(directory, base.path().join(PRIVATE_DIR_NAME));
            let metadata = std::fs::symlink_metadata(&directory).expect("directory should stat");
            assert!(metadata.is_dir());
            assert_eq!(metadata.mode() & 0o777, PRIVATE_DIR_MODE);
        }

        #[test]
        fn an_existing_owner_only_private_directory_is_reused() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let directory = base.path().join(PRIVATE_DIR_NAME);
            std::fs::create_dir(&directory).expect("directory should create");
            chmod(&directory, PRIVATE_DIR_MODE);
            assert_eq!(
                private_directory_in(base.path(), uid).expect("an owner-only directory is sound"),
                directory
            );
        }

        #[test]
        fn a_reachable_or_foreign_private_directory_is_refused() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let directory = base.path().join(PRIVATE_DIR_NAME);

            std::fs::create_dir(&directory).expect("directory should create");
            chmod(&directory, 0o755);
            let error = private_directory_in(base.path(), uid)
                .expect_err("a group- and world-reachable directory must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

            chmod(&directory, PRIVATE_DIR_MODE);
            let error = private_directory_in(base.path(), uid.wrapping_add(1))
                .expect_err("a directory owned by another uid must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn a_symlinked_private_directory_is_refused() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let target = base.path().join("elsewhere");
            std::fs::create_dir(&target).expect("target should create");
            chmod(&target, PRIVATE_DIR_MODE);
            std::os::unix::fs::symlink(&target, base.path().join(PRIVATE_DIR_NAME))
                .expect("symlink should create");

            let error = private_directory_in(base.path(), uid)
                .expect_err("a symlink must not stand in for the private directory");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }
}
