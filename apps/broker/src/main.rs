//! Privileged provider broker process.
//!
//! Owns the control plane and serves capability discovery and bounded provider
//! lifecycles to authenticated local peers over a Unix domain socket. No raw
//! shell, Registry path, or hardware primitive crosses this boundary.
//!
//! Only the Unix domain socket transport is implemented; the Windows named-pipe
//! transport is deliberately out of scope, so the binary refuses to run there.
//!
//! Unless `--socket`/`--journal` or their environment overrides name a path, the
//! socket and the journal live in an owner-only directory under
//! `$XDG_RUNTIME_DIR` (or `/run`), never beside the inherited working directory.
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
    use std::fs::DirBuilder;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use fpsmaxxing_broker::{BrokerService, OwnershipLedger, serve, spawn_service};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport};
    use fpsmaxxing_mock_provider::MockProvider;
    use thiserror::Error;

    /// Flags this binary accepts, in `--help` order.
    const FLAGS: [&str; 2] = ["--socket", "--journal"];

    /// Directory the broker keeps its socket and journal in by default.
    const PRIVATE_DIR_NAME: &str = "fpsmaxxing";

    /// Where [`PRIVATE_DIR_NAME`] lives when `XDG_RUNTIME_DIR` is unset.
    const FALLBACK_RUNTIME_BASE: &str = "/run";

    /// The only mode the broker's private directory may have: owner access only.
    const PRIVATE_DIR_MODE: u32 = 0o700;

    /// Socket file name inside the broker's private directory.
    const DEFAULT_SOCKET_NAME: &str = "broker.sock";

    /// Journal file name inside the broker's private directory.
    const DEFAULT_JOURNAL_NAME: &str = "journal.sqlite";

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
        /// An argument is not one of [`FLAGS`].
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

    pub async fn run() -> Result<(), Box<dyn Error>> {
        let options = parse_args(env::args().skip(1))?;
        let authorizer = SameUidAuthorizer::for_current_process();
        let broker_uid = authorizer.expected_uid();
        let (socket_path, journal_path) = resolve_paths(options, broker_uid)?;

        let ledger = Arc::new(OwnershipLedger::new());
        let broker = spawn_service(move || {
            let provider = Box::new(MockProvider::new(0));
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
    /// An explicit flag wins over the matching environment variable, and either
    /// is taken verbatim: the operator has named a path they control. Anything
    /// left unset falls back into [`private_directory`] rather than beside the
    /// inherited working directory, so a privileged daemon never places its IPC
    /// endpoint or its durable audit journal somewhere it does not own.
    fn resolve_paths(options: Options, broker_uid: u32) -> io::Result<(PathBuf, PathBuf)> {
        let socket = options
            .socket
            .or_else(|| env::var("FPSMAXXING_BROKER_SOCKET").ok());
        let journal = options
            .journal
            .or_else(|| env::var("FPSMAXXING_JOURNAL_PATH").ok());
        match (socket, journal) {
            (Some(socket), Some(journal)) => Ok((PathBuf::from(socket), PathBuf::from(journal))),
            (socket, journal) => {
                let directory = private_directory(broker_uid)?;
                Ok((
                    socket.map_or_else(|| directory.join(DEFAULT_SOCKET_NAME), PathBuf::from),
                    journal.map_or_else(|| directory.join(DEFAULT_JOURNAL_NAME), PathBuf::from),
                ))
            }
        }
    }

    /// Returns the broker's private directory, creating it when it is absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, is not a directory,
    /// is not owned by `broker_uid`, or is not mode [`PRIVATE_DIR_MODE`] - a
    /// parent another user may write is what makes the socket path raceable at
    /// bind time and the journal readable, so an unsound one fails closed.
    fn private_directory(broker_uid: u32) -> io::Result<PathBuf> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| PathBuf::from(FALLBACK_RUNTIME_BASE), PathBuf::from);
        private_directory_in(&base, broker_uid)
    }

    /// Creates and vets [`PRIVATE_DIR_NAME`] under `base`.
    ///
    /// A directory the broker created is vetted too: the inherited umask can
    /// strip bits from the requested mode, and the entry may already have been
    /// something else.
    fn private_directory_in(base: &Path, broker_uid: u32) -> io::Result<PathBuf> {
        let directory = base.join(PRIVATE_DIR_NAME);
        match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(&directory) {
            Ok(()) => {}
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

    /// Parses `--flag value` and `--flag=value` pairs, rejecting anything else.
    ///
    /// In the separated form the value may not itself look like a flag, so
    /// `--socket --journal /var/j.sqlite` is a mistyped command line rather than
    /// a socket literally named `--journal`. Use `--socket=--journal` to mean it.
    ///
    /// # Errors
    ///
    /// Returns [`ArgError`] for an unrecognized argument, a repeated flag, or a
    /// flag whose value is missing, empty, or another flag.
    pub fn parse_args<I>(arguments: I) -> Result<Options, ArgError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut options = Options::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
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
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| ArgError::MissingValue(flag.clone()))?,
            };
            if value.is_empty() {
                return Err(ArgError::MissingValue(flag));
            }
            *slot = Some(value);
        }
        Ok(options)
    }

    #[cfg(test)]
    mod tests {
        use std::io;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::Path;

        use super::{
            ArgError, Options, PRIVATE_DIR_MODE, PRIVATE_DIR_NAME, parse_args, private_directory_in,
        };

        fn parse(arguments: &[&str]) -> Result<Options, ArgError> {
            parse_args(arguments.iter().map(|argument| (*argument).to_owned()))
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

        #[test]
        fn both_flag_forms_are_accepted() {
            let options = parse(&["--socket", "/run/b.sock", "--journal=/var/j.sqlite"])
                .expect("both forms should parse");
            assert_eq!(
                options,
                Options {
                    socket: Some("/run/b.sock".to_owned()),
                    journal: Some("/var/j.sqlite".to_owned()),
                }
            );
            assert_eq!(
                parse(&[]).expect("no arguments should parse"),
                Options::default()
            );
        }

        #[test]
        fn a_flag_shaped_value_needs_the_inline_form() {
            assert!(matches!(
                parse(&["--socket", "--journal", "/var/j.sqlite"])
                    .expect_err("a swallowed flag must not become the socket path"),
                ArgError::MissingValue(_)
            ));
            let options = parse(&["--socket=--journal"]).expect("the inline form is explicit");
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
