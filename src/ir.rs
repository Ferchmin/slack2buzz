//! The intermediate representation written by `parse` and read by `emit`.
//!
//! `import.jsonl` is one JSON object per line, each tagged with `type`. The
//! first line is always a [`Header`]; the rest may appear in any order, though
//! `parse` emits them grouped by channel and ascending by timestamp so a
//! human can read the file top to bottom.
//!
//! The IR is the contract between the two stages and is versioned
//! independently of the tool. `emit` refuses an IR whose
//! [`Header::ir_version`] it does not recognise rather than guessing.
//!
//! Nothing in here is Buzz-shaped. It describes what Slack said, normalised —
//! kinds, pubkeys, and signatures are `emit`'s concern. That separation is
//! what lets a Discord or Teams parser target the same file.

use serde::{Deserialize, Serialize};

/// IR format version. Bump on any breaking change to the record shapes.
pub const IR_VERSION: u32 = 1;

/// One line of `import.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Header(Header),
    User(User),
    Channel(Channel),
    Message(Message),
    Reaction(Reaction),
    File(FileRef),
    Emoji(Emoji),
}

/// First line of the IR: what produced this file and what it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub ir_version: u32,
    /// `slack2buzz 0.1.0` — informational, not a compatibility signal.
    pub generator: String,
    /// Always `slack` today. A future importer sets its own source.
    pub source: String,
    /// Slack team id, when the export reveals it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Channels the operator selected, by Slack id. `emit` treats this as the
    /// authoritative scope of the import, so a re-run with a different
    /// selection is a different import.
    pub selected_channels: Vec<String>,
    /// Channels present in the export but deliberately not parsed.
    pub skipped_channels: Vec<String>,
    /// Counts of each record type that follows, for cheap sanity checks.
    pub counts: Counts,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub users: usize,
    pub channels: usize,
    pub messages: usize,
    pub thread_replies: usize,
    pub reactions: usize,
    pub files: usize,
    pub emoji: usize,
    /// Messages dropped as join/leave noise (see `--keep-joins`).
    pub dropped_joins: usize,
    /// Messages skipped because we could not make sense of them. Non-zero
    /// here is a fidelity loss and `parse` reports it on stderr.
    pub skipped_unparseable: usize,
}

/// A Slack member. Bots and deleted users are included — their messages are
/// in the history and dropping the user record would orphan them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub slack_id: String,
    /// Slack's `name` (the handle, e.g. `pawel`).
    pub name: String,
    /// Best available human name, in Slack's own preference order:
    /// `profile.display_name`, then `profile.real_name`, then `name`.
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub is_bot: bool,
    pub is_deleted: bool,
}

/// What kind of conversation a channel is. Drives both the Buzz channel
/// visibility on emit and what `probe` tells the operator they actually have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// `channels.json` — public channel.
    Public,
    /// `groups.json` — private channel.
    Private,
    /// `dms.json` — two-person direct message.
    Dm,
    /// `mpims.json` — multi-person direct message.
    GroupDm,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Dm => "dm",
            Self::GroupDm => "group_dm",
        }
    }

    /// Whether this kind carries an expectation of privacy. `probe` and
    /// `emit` both warn louder for these.
    pub fn is_private(self) -> bool {
        !matches!(self, Self::Public)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub slack_id: String,
    /// Slack channel name. Absent for DMs, which Slack does not name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ChannelKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Slack user id of the creator, when the export records one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    pub is_archived: bool,
    pub members: Vec<String>,
}

/// A message. One record per Slack message, thread roots and replies alike —
/// `thread_ts` is what distinguishes them, and `emit` uses it to order its two
/// sub-passes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Slack's `ts`, verbatim (`"1728394855.123456"`). This is the identity of
    /// the message within its channel and the ledger's idempotency key —
    /// never reformat it.
    pub slack_ts: String,
    pub channel_slack_id: String,
    /// Unix seconds derived from `slack_ts`. What `emit` puts in `created_at`.
    pub created_at: i64,
    /// Author. `None` for messages the export attributes to no user (some
    /// subtypes, and bot messages that predate `bot_id` attribution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_slack_id: Option<String>,
    /// Set when the message came from a bot/app rather than a member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    /// Display name to attribute to, when the message carries its own
    /// (`username` on bot messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_override: Option<String>,
    /// Normalised text: mrkdwn converted, entities decoded, refs rewritten.
    pub text: String,
    /// Slack's original `text`, kept verbatim. Costs bytes, buys the ability
    /// to re-normalise without re-exporting from Slack, and makes any
    /// normalisation bug auditable after the fact.
    pub raw_text: String,
    /// Thread root's `ts`. `None` for un-threaded messages. Equal to
    /// `slack_ts` when this message *is* the root of a thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// Slack `subtype` (`channel_topic`, `bot_message`, `file_share`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// Unix seconds of the last edit, when the message was edited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<i64>,
    /// Slack user ids mentioned, in order of first appearance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// Slack file ids attached, resolvable against the [`FileRef`] records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_ids: Vec<String>,
}

impl Message {
    /// A message is a thread root when it has a `thread_ts` equal to its own
    /// `ts`. Slack sets `thread_ts` on both the root and every reply.
    pub fn is_thread_root(&self) -> bool {
        self.thread_ts.as_deref() == Some(self.slack_ts.as_str())
    }

    /// A reply hangs off a different message's `thread_ts`.
    pub fn is_thread_reply(&self) -> bool {
        matches!(&self.thread_ts, Some(t) if t != &self.slack_ts)
    }
}

/// One reactor's one emoji on one message. Slack stores reactions grouped by
/// emoji with a user list; we explode them so each becomes its own kind:7
/// event signed by that reactor's derived key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    pub channel_slack_id: String,
    /// `slack_ts` of the message reacted to.
    pub target_slack_ts: String,
    /// Emoji short name without colons (`thumbsup`, `custom-parrot`).
    pub name: String,
    pub user_slack_id: String,
}

/// A file *reference*. Slack exports link to files; they do not contain them.
/// Whether the bytes still exist is unknown until `emit --with-files` tries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRef {
    pub slack_file_id: String,
    pub channel_slack_id: String,
    /// `slack_ts` of the message the file was shared in.
    pub message_slack_ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Requires an authenticated Slack token with file scope to fetch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_private: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    /// Slack already told us the file is gone. Saves a doomed fetch.
    pub is_deleted: bool,
    /// Hosted elsewhere (Google Drive, a pasted link); there are no bytes to
    /// migrate, only a URL to preserve.
    pub is_external: bool,
}

/// A workspace custom emoji.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Emoji {
    /// Short name without colons.
    pub name: String,
    /// Either a URL to the image or `alias:othername`.
    pub url: String,
}

/// Split a Slack `ts` into unix seconds.
///
/// Slack timestamps are `"<seconds>.<microseconds>"`. The fractional part is a
/// per-channel disambiguator, not real sub-second precision, so it is kept
/// only in `slack_ts` and dropped from `created_at`.
pub fn ts_to_unix_secs(ts: &str) -> Option<i64> {
    let secs = ts.split('.').next()?;
    secs.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_parses_seconds_and_ignores_microseconds() {
        assert_eq!(ts_to_unix_secs("1728394855.123456"), Some(1728394855));
        assert_eq!(ts_to_unix_secs("1728394855"), Some(1728394855));
        assert_eq!(ts_to_unix_secs("not-a-ts"), None);
        assert_eq!(ts_to_unix_secs(""), None);
    }

    fn msg(ts: &str, thread_ts: Option<&str>) -> Message {
        Message {
            slack_ts: ts.to_string(),
            channel_slack_id: "C1".into(),
            created_at: 0,
            user_slack_id: None,
            bot_id: None,
            author_override: None,
            text: String::new(),
            raw_text: String::new(),
            thread_ts: thread_ts.map(str::to_string),
            subtype: None,
            edited_at: None,
            mentions: vec![],
            file_ids: vec![],
        }
    }

    #[test]
    fn thread_root_and_reply_are_distinguished_by_thread_ts() {
        let plain = msg("100.1", None);
        assert!(!plain.is_thread_root());
        assert!(!plain.is_thread_reply());

        let root = msg("100.1", Some("100.1"));
        assert!(root.is_thread_root());
        assert!(!root.is_thread_reply());

        let reply = msg("200.2", Some("100.1"));
        assert!(!reply.is_thread_root());
        assert!(reply.is_thread_reply());
    }
}
