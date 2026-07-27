//! Privileged provider broker process.
//!
//! Owns the control plane and serves capability discovery and bounded provider
//! lifecycles to authenticated local peers over a Unix domain socket. No raw
//! shell, Registry path, or hardware primitive crosses this boundary.
//!
//! Only the Unix domain socket transport is implemented; the Windows named-pipe
//! transport is deliberately out of scope, so the binary refuses to run there.
//!
//! Unless `--socket`/`--journal` or their environment overrides
//! (`FPSMAXXING_BROKER_SOCKET` and `FPSMAXXING_BROKER_JOURNAL_PATH`, both
//! broker-only) name a path, the socket and the journal live in an owner-only
//! directory under `$XDG_RUNTIME_DIR` (or `/run`), never beside the inherited
//! working directory. Wherever a path came from, it is held to the same bar
//! before it is used: absolute, under a parent chain no other user can write.
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
    use std::fs::{DirBuilder, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use fpsmaxxing_broker::{BrokerService, OwnershipLedger, serve, spawn_service};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport};
    use fpsmaxxing_mock_provider::MockProvider;
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

A flag wins over its environment variable. Unset, each falls back into an
owner-only directory under $XDG_RUNTIME_DIR (or /run when it is unset, is not
absolute, or the broker runs as root). Every path must be absolute and sit in
an existing directory no other user can write, under a parent chain owned by
the broker or root, or the broker refuses to start.";

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
        let options = match parse_args(env::args().skip(1))? {
            Invocation::Help => {
                println!("{USAGE}");
                return Ok(());
            }
            Invocation::Run(options) => options,
        };
        let authorizer = SameUidAuthorizer::for_current_process();
        let broker_uid = authorizer.expected_uid();
        let (socket_path, journal_path) = resolve_paths(options, broker_uid)?;

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

    /// Resolves the socket and journal locations, in override order.
    ///
    /// An explicit flag wins over the matching environment variable. Anything
    /// left unset falls back into [`private_directory`] rather than beside the
    /// inherited working directory, so a privileged daemon never places its IPC
    /// endpoint or its durable audit journal somewhere it does not own.
    ///
    /// The environment is read with [`env::var_os`] rather than `env::var`, so a
    /// path that is not UTF-8 relocates the socket or journal as configured
    /// instead of being silently dropped back to the default.
    fn resolve_paths(options: Options, broker_uid: u32) -> io::Result<(PathBuf, PathBuf)> {
        resolve_paths_from(options, broker_uid, |name| env::var_os(name))
    }

    /// [`resolve_paths`] against an arbitrary environment lookup.
    ///
    /// Only [`SOCKET_ENV`] and [`JOURNAL_ENV`] are ever consulted; the broker
    /// shares no path variable with the unprivileged gateway or CLI.
    ///
    /// Every resolved path is put through [`vet_resolved_path`], whatever named
    /// it. An environment variable is inherited from whoever started the broker,
    /// so honoring one verbatim would hand that caller the choice of where a
    /// root-owned socket and audit journal are created - exactly what
    /// [`runtime_base`] refuses them for `XDG_RUNTIME_DIR`.
    fn resolve_paths_from<F>(
        options: Options,
        broker_uid: u32,
        lookup: F,
    ) -> io::Result<(PathBuf, PathBuf)>
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
        let (socket, journal) = match (socket, journal) {
            (Some(socket), Some(journal)) => (PathBuf::from(socket), PathBuf::from(journal)),
            (socket, journal) => {
                let directory = private_directory(broker_uid)?;
                (
                    socket.map_or_else(|| directory.join(DEFAULT_SOCKET_NAME), PathBuf::from),
                    journal.map_or_else(|| directory.join(DEFAULT_JOURNAL_NAME), PathBuf::from),
                )
            }
        };
        vet_resolved_path(&socket, broker_uid)?;
        vet_resolved_path(&journal, broker_uid)?;
        Ok((socket, journal))
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
    /// than the ancestors above it: no other user may write it, sticky or not.
    /// Sticky stops another user renaming or removing the broker's socket or
    /// journal, but not creating that entry first in a shared directory like
    /// `/tmp` and keeping ownership of the file a privileged broker then writes
    /// every `apply-intent` record into. Higher up, creating an entry is not
    /// the threat - swapping a vetted directory is - and sticky does prevent
    /// that.
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
        vet_directory(parent, broker_uid, Sticky::Insufficient)?;
        match parent.parent() {
            Some(above) => vet_ancestors(above, broker_uid),
            None => Ok(()),
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
        OpenOptions::new()
            .create(true)
            .write(true)
            .mode(JOURNAL_FILE_MODE)
            .open(path)?;
        std::fs::set_permissions(path, Permissions::from_mode(JOURNAL_FILE_MODE))?;
        for suffix in JOURNAL_SIDE_SUFFIXES {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            match std::fs::set_permissions(
                Path::new(&side),
                Permissions::from_mode(JOURNAL_FILE_MODE),
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Returns the broker's private directory, creating it when it is absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory or any of its ancestors is unsound;
    /// see [`private_directory_in`].
    fn private_directory(broker_uid: u32) -> io::Result<PathBuf> {
        let base = runtime_base(env::var_os("XDG_RUNTIME_DIR").as_deref(), broker_uid);
        private_directory_in(&base, broker_uid)
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
                std::fs::set_permissions(&directory, Permissions::from_mode(PRIVATE_DIR_MODE))?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
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

    /// Whether the sticky bit redeems a group- or world-writable directory.
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Sticky {
        /// Sticky is enough: only an entry's owner may rename or remove it, so
        /// the directory below cannot be swapped for one the broker does not
        /// own. This is how `/tmp` and similar shared roots are protected.
        Redeems,
        /// Sticky is not enough: it does not stop another user creating an
        /// entry the broker is about to create itself, and the creator keeps
        /// ownership of it.
        Insufficient,
    }

    /// Refuses a directory another user could tamper with.
    ///
    /// It must be a real directory - not a symlink - owned by the broker or by
    /// root, and not writable by group or world unless `sticky` redeems it.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] if the directory fails the
    /// bar, or the underlying error - named with the directory, since a path
    /// the broker will not create is the likeliest reason it cannot be
    /// inspected - if it cannot be inspected at all.
    fn vet_directory(directory: &Path, broker_uid: u32, sticky: Sticky) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{} cannot be inspected: {error}", directory.display()),
            )
        })?;
        let mode = metadata.mode();
        let owned_by_trusted_uid = metadata.uid() == broker_uid || metadata.uid() == ROOT_UID;
        let redeemed = sticky == Sticky::Redeems && mode & STICKY_BIT != 0;
        let writable_by_others = mode & OTHER_WRITE_BITS != 0 && !redeemed;
        if !metadata.is_dir() || !owned_by_trusted_uid || writable_by_others {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a directory owned by uid {broker_uid} or root that no other user can write",
                    directory.display()
                ),
            ));
        }
        Ok(())
    }

    /// Refuses a base whose own path another user could tamper with.
    ///
    /// Every component from `base` up to `/` goes through [`vet_directory`],
    /// where a sticky directory is sound: the threat an ancestor carries is the
    /// swap of the directory below it, which sticky prevents.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] for the first component that
    /// fails, or the underlying error if a component cannot be inspected.
    fn vet_ancestors(base: &Path, broker_uid: u32) -> io::Result<()> {
        for ancestor in base.ancestors() {
            vet_directory(ancestor, broker_uid, Sticky::Redeems)?;
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
    /// # Errors
    ///
    /// Returns [`ArgError`] for an unrecognized argument, a repeated flag, or a
    /// flag whose value is missing, empty, or another flag.
    pub fn parse_args<I>(arguments: I) -> Result<Invocation, ArgError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut options = Options::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
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
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::Path;

        use fpsmaxxing_control_plane::ControlPlane;
        use fpsmaxxing_mock_provider::MockProvider;

        use super::{
            ArgError, FALLBACK_RUNTIME_BASE, HELP_FLAGS, Invocation, JOURNAL_ENV,
            JOURNAL_FILE_MODE, Options, PRIVATE_DIR_MODE, PRIVATE_DIR_NAME, ROOT_UID, SOCKET_ENV,
            parse_args, private_directory_in, resolve_paths_from, restrict_journal, runtime_base,
        };

        /// The variable the unprivileged gateway and CLI use for their journal.
        const GATEWAY_JOURNAL_ENV: &str = "FPSMAXXING_JOURNAL_PATH";

        /// An unprivileged uid, so `XDG_RUNTIME_DIR` is eligible at all.
        const SESSION_UID: u32 = 1000;

        fn parse(arguments: &[&str]) -> Result<Invocation, ArgError> {
            parse_args(arguments.iter().map(|argument| (*argument).to_owned()))
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
        fn the_journal_never_comes_from_the_gateway_environment() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let seen = RefCell::new(BTreeSet::new());
            let (socket, journal) = resolve_paths_from(
                Options::default(),
                uid,
                recording_lookup(base.path(), &seen),
            )
            .expect("both paths come from the environment, so no directory is touched");

            assert_eq!(socket, base.path().join(SOCKET_ENV));
            assert_eq!(journal, base.path().join(JOURNAL_ENV));
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
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let seen = RefCell::new(BTreeSet::new());
            let options = Options {
                socket: Some(path_string(&base.path().join("b.sock"))),
                journal: Some(path_string(&base.path().join("j.sqlite"))),
            };
            let (socket, journal) =
                resolve_paths_from(options, uid, recording_lookup(base.path(), &seen))
                    .expect("explicit flags need no directory");
            assert_eq!(socket, base.path().join("b.sock"));
            assert_eq!(journal, base.path().join("j.sqlite"));
            assert!(
                seen.borrow().is_empty(),
                "a flag must not consult the environment at all"
            );
        }

        #[test]
        fn a_relative_path_from_the_environment_is_refused() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let options = Options {
                journal: Some(path_string(&base.path().join("j.sqlite"))),
                ..Options::default()
            };
            let error =
                resolve_paths_from(options, uid, socket_env_lookup(Path::new("broker.sock")))
                    .expect_err("a relative override would land beside the inherited cwd");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn a_path_from_the_environment_under_a_writable_parent_is_refused() {
            let outer = tempfile::tempdir().expect("temporary directory should exist");
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
            let (socket, _journal) = resolve_paths_from(
                options,
                uid,
                socket_env_lookup(&reachable.join("broker.sock")),
            )
            .expect("an owner-only parent is sound");
            assert_eq!(socket, reachable.join("broker.sock"));
        }

        #[test]
        fn a_sticky_directory_may_not_hold_the_socket_or_the_journal() {
            let outer = tempfile::tempdir().expect("temporary directory should exist");
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
            let error = resolve_paths_from(options, uid, |_| None)
                .expect_err("a sticky world-writable parent must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

            let options = Options {
                socket: Some(path_string(&outer.path().join("b.sock"))),
                journal: Some(path_string(&shared.join("j.sqlite"))),
            };
            let error = resolve_paths_from(options, uid, |_| None)
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
            let (socket, journal) = resolve_paths_from(options, uid, |_| None)
                .expect("an owner-only directory under a sticky ancestor is sound");
            assert_eq!(socket, private.join("broker.sock"));
            assert_eq!(journal, private.join("j.sqlite"));
        }

        #[test]
        fn a_flag_path_is_vetted_the_same_way_as_the_environment() {
            let base = tempfile::tempdir().expect("temporary directory should exist");
            let uid = own_uid(base.path());
            let options = Options {
                socket: Some("broker.sock".to_owned()),
                journal: Some(path_string(&base.path().join("j.sqlite"))),
            };
            let error = resolve_paths_from(options, uid, |_| None)
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
