//! The Slack side of `invite`: sending a DM.
//!
//! A trait rather than a concrete client, for one reason that matters more than
//! testability: it makes the dry-run path and the real path structurally
//! identical. [`DryRunMessenger`] and the live client are the same code with a
//! different implementation swapped in, so a dry run genuinely exercises the
//! sequencing, the ledger writes, and the ordering — not a separate branch that
//! happens to print instead.
//!
//! Only one Slack call is needed. `chat.postMessage` accepts a user id as its
//! `channel` and opens the DM implicitly, so there is no `conversations.open`,
//! no `users.list`, and no email address anywhere in this flow.
//!
//! Required token scopes: `chat:write` (and `im:write` on some app
//! configurations). Notably *not* `users:read.email`.

use anyhow::Result;

/// Where a sent DM landed, for the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct SentDm {
    /// Slack DM channel id the message went to.
    pub channel: String,
    /// Slack `ts` of the posted message.
    pub ts: String,
}

/// Sends a direct message to one Slack user.
pub trait Messenger {
    /// Post `body` to `slack_user_id` as a DM.
    ///
    /// Implementations must treat a Slack `ok: false` response as an error, and
    /// must surface `ratelimited` distinguishably so the caller can back off
    /// rather than burning through the remaining recipients.
    fn send_dm(&mut self, slack_user_id: &str, body: &str) -> Result<SentDm>;
}

/// Sends nothing. The default, and what `--dry-run` uses.
///
/// Records what *would* have been sent so the caller can print it and so tests
/// can assert on ordering and content.
#[derive(Debug, Default)]
pub struct DryRunMessenger {
    pub sent: Vec<(String, String)>,
}

impl Messenger for DryRunMessenger {
    fn send_dm(&mut self, slack_user_id: &str, body: &str) -> Result<SentDm> {
        self.sent
            .push((slack_user_id.to_string(), body.to_string()));
        Ok(SentDm {
            channel: format!("DRYRUN-{slack_user_id}"),
            ts: "0.000000".to_string(),
        })
    }
}

/// A messenger that fails for chosen users, so retry and partial-failure
/// reporting can be tested.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FlakyMessenger {
    pub fail_for: Vec<String>,
    pub sent: Vec<String>,
}

#[cfg(test)]
impl Messenger for FlakyMessenger {
    fn send_dm(&mut self, slack_user_id: &str, _body: &str) -> Result<SentDm> {
        if self.fail_for.iter().any(|f| f == slack_user_id) {
            anyhow::bail!("slack: ratelimited");
        }
        self.sent.push(slack_user_id.to_string());
        Ok(SentDm {
            channel: format!("D-{slack_user_id}"),
            ts: "1.0".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    // A panic IS the failure report in a test; Buzz's CONTRIBUTING allows it.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn dry_run_records_instead_of_sending() {
        let mut m = DryRunMessenger::default();
        m.send_dm("U0ALICE", "hello").unwrap();
        assert_eq!(m.sent, vec![("U0ALICE".to_string(), "hello".to_string())]);
    }

    #[test]
    fn flaky_messenger_fails_only_for_named_users() {
        let mut m = FlakyMessenger {
            fail_for: vec!["U0BOB".to_string()],
            sent: vec![],
        };
        assert!(m.send_dm("U0ALICE", "x").is_ok());
        assert!(m.send_dm("U0BOB", "x").is_err());
        assert_eq!(m.sent, vec!["U0ALICE"]);
    }
}
