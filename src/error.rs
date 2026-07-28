//! Library error type.
//!
//! Buzz's convention, from its CONTRIBUTING: `thiserror` for library error
//! types, `anyhow` only for binary-level propagation. Every one of its library
//! crates follows that and none of them depend on `anyhow`, so this crate's
//! library surface returns [`Error`] and only `main.rs` reaches for `anyhow`.
//!
//! The payoff is not just convention. Exit codes are derived from the error
//! *variant* via [`Error::exit_code`], rather than by matching substrings of a
//! formatted message — which is what this crate did before, and which silently
//! reclassifies an error the moment someone rewords it.

use thiserror::Error;

/// Result alias for the library surface.
pub type Result<T> = std::result::Result<T, Error>;

/// Exit codes, mirroring Buzz's CLI discipline.
pub mod exit {
    pub const OK: i32 = 0;
    pub const INPUT: i32 = 1;
    pub const NETWORK: i32 = 2;
    pub const AUTH: i32 = 3;
    pub const OTHER: i32 = 4;
    pub const WRITE_CONFLICT: i32 = 5;
}

#[derive(Debug, Error)]
pub enum Error {
    // ── reading the export ───────────────────────────────────────────────────
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("reading {path} as a zip archive")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },

    #[error("parsing {what} from the export")]
    Json {
        what: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("{path}:{line}: malformed IR record")]
    MalformedIr {
        path: String,
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    #[error("serialising an IR record")]
    Serialise(#[source] serde_json::Error),

    // ── ledger ───────────────────────────────────────────────────────────────
    #[error("ledger: {context}")]
    Ledger {
        context: String,
        #[source]
        source: rusqlite::Error,
    },

    // ── channel selection ────────────────────────────────────────────────────
    #[error(
        "no channels selected: pass --all, --all-public, --channels <names>, \
         or run without --no-input to choose interactively"
    )]
    NoChannelsSelected,

    #[error(
        "no channel in this export matches \"{selector}\" — \
         run `slack2buzz probe` to list what is available"
    )]
    UnknownChannel { selector: String },

    #[error("no channel in this export matches --exclude \"{selector}\"")]
    UnknownExcludedChannel { selector: String },

    #[error("the selection matched no conversations with messages to import")]
    NoConversationsMatched,

    #[error("this export contains no conversations with messages")]
    NoConversations,

    #[error("nothing selected")]
    NothingSelected,

    #[error("selected conversation {id} is not in the inventory")]
    UnknownSelectedConversation { id: String },

    // ── invite ───────────────────────────────────────────────────────────────
    #[error(
        "no one in this import matches {flag} \"{selector}\" — \
         run `slack2buzz invite --list` to see the candidates"
    )]
    UnknownPerson { flag: String, selector: String },

    #[error("no invitable people in this import (all bots or deactivated)")]
    NoInvitablePeople,

    #[error("nobody selected")]
    NobodySelected,

    #[error("{path} contains no user records")]
    NoUserRecords { path: String },

    /// The authenticated key cannot mint invites. Distinct from a generic
    /// failure because it maps to the auth exit code, and because it is worth
    /// catching before the first DM rather than after the fortieth.
    #[error("this key holds role {role} in {community}; only owners and admins can mint invites")]
    InsufficientRole { role: String, community: String },

    // ── interactive ──────────────────────────────────────────────────────────
    #[error("prompt failed")]
    Prompt(#[from] dialoguer::Error),

    // ── remote services ──────────────────────────────────────────────────────
    #[error("slack: {0}")]
    Slack(String),

    #[error("relay: {0}")]
    Relay(String),

    /// Something the operator asked for that this build cannot do yet.
    #[error("{0}")]
    NotImplemented(String),
}

/// Bare `?` on a rusqlite call. Most ledger statements are self-describing from
/// the surrounding function name, so they get a generic context; the ones where
/// the row identity matters build [`Error::Ledger`] explicitly with it.
impl From<rusqlite::Error> for Error {
    fn from(source: rusqlite::Error) -> Self {
        Self::Ledger {
            context: "database operation failed".into(),
            source,
        }
    }
}

impl Error {
    /// Convenience for the many `std::io` call sites that want to name what
    /// they were doing.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// The process exit code this error should produce.
    ///
    /// Derived from the variant, so rewording a message can never silently move
    /// an error into a different class.
    pub fn exit_code(&self) -> i32 {
        match self {
            // Anything the operator can fix by changing flags or input.
            Self::Io { .. }
            | Self::Zip { .. }
            | Self::Json { .. }
            | Self::MalformedIr { .. }
            | Self::NoChannelsSelected
            | Self::UnknownChannel { .. }
            | Self::UnknownExcludedChannel { .. }
            | Self::NoConversationsMatched
            | Self::NoConversations
            | Self::NothingSelected
            | Self::UnknownPerson { .. }
            | Self::NoInvitablePeople
            | Self::NobodySelected
            | Self::NoUserRecords { .. } => exit::INPUT,

            Self::Slack(_) | Self::Relay(_) => exit::NETWORK,

            Self::InsufficientRole { .. } => exit::AUTH,

            Self::Serialise(_)
            | Self::Ledger { .. }
            | Self::UnknownSelectedConversation { .. }
            | Self::Prompt(_)
            | Self::NotImplemented(_) => exit::OTHER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_fixable_errors_are_input_errors() {
        assert_eq!(Error::NoChannelsSelected.exit_code(), exit::INPUT);
        assert_eq!(
            Error::UnknownChannel {
                selector: "genral".into()
            }
            .exit_code(),
            exit::INPUT
        );
        assert_eq!(
            Error::UnknownPerson {
                flag: "--users".into(),
                selector: "alise".into()
            }
            .exit_code(),
            exit::INPUT
        );
    }

    #[test]
    fn remote_failures_are_network_errors() {
        assert_eq!(
            Error::Slack("ratelimited".into()).exit_code(),
            exit::NETWORK
        );
        assert_eq!(Error::Relay("503".into()).exit_code(), exit::NETWORK);
    }

    #[test]
    fn an_underprivileged_key_is_an_auth_error() {
        assert_eq!(
            Error::InsufficientRole {
                role: "member".into(),
                community: "Acme".into()
            }
            .exit_code(),
            exit::AUTH
        );
    }

    /// The messages are operator-facing, so they must say what to do next.
    #[test]
    fn selection_errors_name_the_command_that_helps() {
        let e = Error::UnknownChannel {
            selector: "genral".into(),
        };
        assert!(e.to_string().contains("slack2buzz probe"));

        let e = Error::UnknownPerson {
            flag: "--users".into(),
            selector: "alise".into(),
        };
        assert!(e.to_string().contains("--list"));
    }

    /// `#[source]` must be preserved so `{:#}` shows the underlying cause
    /// rather than swallowing it.
    #[test]
    fn io_errors_keep_their_cause() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e = Error::io("reading export.zip", inner);
        assert_eq!(e.to_string(), "reading export.zip");
        assert!(std::error::Error::source(&e).is_some());
    }
}
