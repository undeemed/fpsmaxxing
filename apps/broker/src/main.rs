//! Privileged provider broker process.
//!
//! Owns the control plane and serves capability discovery and bounded provider
//! lifecycles to authenticated local peers over a Unix domain socket. No raw
//! shell, Registry path, or hardware primitive crosses this boundary.
//!
//! Only the Unix domain socket transport is implemented; the Windows named-pipe
//! transport is deliberately out of scope, so the binary refuses to run there.
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
    use std::io;
    use std::sync::Arc;

    use fpsmaxxing_broker::{BrokerService, OwnershipLedger, serve, spawn_service};
    use fpsmaxxing_control_plane::ControlPlane;
    use fpsmaxxing_ipc::{PeerAuthorizer, SameUidAuthorizer, UnixSocketTransport};
    use fpsmaxxing_mock_provider::MockProvider;
    use thiserror::Error;

    /// Flags this binary accepts, in `--help` order.
    const FLAGS: [&str; 2] = ["--socket", "--journal"];

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
        let socket_path = options
            .socket
            .or_else(|| env::var("FPSMAXXING_BROKER_SOCKET").ok())
            .unwrap_or_else(|| "fpsmaxxing-broker.sock".to_owned());
        let journal_path = options
            .journal
            .or_else(|| env::var("FPSMAXXING_JOURNAL_PATH").ok())
            .unwrap_or_else(|| "fpsmaxxing-journal.sqlite".to_owned());

        let ledger = Arc::new(OwnershipLedger::new());
        let broker = spawn_service(move || {
            let provider = Box::new(MockProvider::new(0));
            let plane = ControlPlane::open(provider, &journal_path).map_err(io::Error::other)?;
            Ok(BrokerService::new(plane, ledger))
        })
        .await?;

        let transport = UnixSocketTransport::bind(&socket_path)?;
        let authorizer = SameUidAuthorizer::for_current_process();
        let broker_uid = authorizer.expected_uid();
        let authorizer: Arc<dyn PeerAuthorizer> = Arc::new(authorizer);
        println!("fpsmaxxing-broker: listening on {socket_path} (same-uid ACL, uid {broker_uid})");
        serve(transport, broker, authorizer).await?;
        Ok(())
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
        use super::{ArgError, Options, parse_args};

        fn parse(arguments: &[&str]) -> Result<Options, ArgError> {
            parse_args(arguments.iter().map(|argument| (*argument).to_owned()))
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
    }
}
