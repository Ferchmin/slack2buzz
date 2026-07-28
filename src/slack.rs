//! Deserialisation of a Slack export, exactly as Slack writes it.
//!
//! These types mirror the on-disk shape and nothing else — no normalisation,
//! no defaulting that invents data. Slack's export format has drifted over the
//! years and varies by workspace plan, so almost every field is optional and
//! unknown fields are ignored rather than rejected: an export from 2016 and one
//! from last week both have to parse.
//!
//! The one thing we are strict about is `ts`. It is the identity of a message
//! and the ledger's idempotency key, so a message without one is unusable and
//! is counted as a skip rather than silently given a synthetic timestamp.

use serde::Deserialize;

/// An entry in `users.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackUser {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub profile: Option<SlackProfile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlackProfile {
    #[serde(default)]
    pub real_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Slack offers several sizes; 192px is the largest reliably present one.
    #[serde(default)]
    pub image_192: Option<String>,
    #[serde(default)]
    pub image_512: Option<String>,
}

impl SlackUser {
    /// Slack's own display preference: `display_name`, then `real_name`, then
    /// the handle. Empty strings count as absent — Slack writes `""` rather
    /// than omitting the key when a user never set a display name.
    pub fn best_display_name(&self) -> String {
        let profile = self.profile.as_ref();
        let candidates = [
            profile.and_then(|p| p.display_name.as_deref()),
            profile.and_then(|p| p.real_name.as_deref()),
            Some(self.name.as_str()),
        ];
        candidates
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .unwrap_or(&self.id)
            .to_string()
    }

    pub fn avatar_url(&self) -> Option<String> {
        let profile = self.profile.as_ref()?;
        profile
            .image_512
            .clone()
            .or_else(|| profile.image_192.clone())
    }
}

/// An entry in `channels.json` / `groups.json` / `mpims.json` / `dms.json`.
///
/// The four files share a shape loosely: DMs have no `name` and use `members`
/// rather than a member list under `value`.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackConversation {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub created: Option<i64>,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub topic: Option<SlackPurpose>,
    #[serde(default)]
    pub purpose: Option<SlackPurpose>,
}

/// Slack wraps topic/purpose in `{value, creator, last_set}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlackPurpose {
    #[serde(default)]
    pub value: String,
}

impl SlackPurpose {
    fn non_empty(&self) -> Option<String> {
        let v = self.value.trim();
        (!v.is_empty()).then(|| v.to_string())
    }
}

impl SlackConversation {
    pub fn topic_text(&self) -> Option<String> {
        self.topic.as_ref().and_then(SlackPurpose::non_empty)
    }
    pub fn purpose_text(&self) -> Option<String> {
        self.purpose.as_ref().and_then(SlackPurpose::non_empty)
    }
}

/// One message from a per-channel `YYYY-MM-DD.json` file.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackMessage {
    /// Missing `ts` makes a message unusable; see module docs.
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default, rename = "type")]
    pub msg_type: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    /// Present on bot/app messages that carry their own display name.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub edited: Option<SlackEdited>,
    #[serde(default)]
    pub reactions: Vec<SlackReaction>,
    #[serde(default)]
    pub files: Vec<SlackFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlackEdited {
    #[serde(default)]
    pub ts: Option<String>,
}

/// Slack groups reactions by emoji with a list of reactors. `parse` explodes
/// these into one IR record per (emoji, reactor).
#[derive(Debug, Clone, Deserialize)]
pub struct SlackReaction {
    pub name: String,
    #[serde(default)]
    pub users: Vec<String>,
}

/// A file *reference*. The export contains no bytes — see `docs/limitations.md`.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackFile {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mimetype: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub url_private: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    /// Slack sets `"hidden_by_limit"` or `"file_deleted"` in `mode` once the
    /// bytes are gone, and some exports carry an explicit flag.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub is_external: bool,
}

impl SlackFile {
    /// Whether Slack has already told us the bytes are unavailable, so
    /// `emit --with-files` can skip a fetch that is guaranteed to fail.
    pub fn is_gone(&self) -> bool {
        matches!(
            self.mode.as_deref(),
            Some("hidden_by_limit") | Some("file_deleted") | Some("tombstone")
        )
    }
}

/// Subtypes that represent membership churn rather than conversation. Dropped
/// unless `--keep-joins` is given.
pub const JOIN_LEAVE_SUBTYPES: &[&str] =
    &["channel_join", "channel_leave", "group_join", "group_leave"];

pub fn is_join_leave(subtype: Option<&str>) -> bool {
    subtype.is_some_and(|s| JOIN_LEAVE_SUBTYPES.contains(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_json(json: &str) -> SlackUser {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn display_name_follows_slack_preference_order() {
        let u = user_json(
            r#"{"id":"U1","name":"handle","profile":{"display_name":"Disp","real_name":"Real"}}"#,
        );
        assert_eq!(u.best_display_name(), "Disp");
    }

    #[test]
    fn empty_display_name_falls_through_to_real_name() {
        let u = user_json(
            r#"{"id":"U1","name":"handle","profile":{"display_name":"","real_name":"Real"}}"#,
        );
        assert_eq!(u.best_display_name(), "Real");
    }

    #[test]
    fn missing_profile_falls_back_to_handle() {
        let u = user_json(r#"{"id":"U1","name":"handle"}"#);
        assert_eq!(u.best_display_name(), "handle");
    }

    #[test]
    fn nameless_user_falls_back_to_id() {
        let u = user_json(r#"{"id":"U1"}"#);
        assert_eq!(u.best_display_name(), "U1");
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        let u = user_json(r#"{"id":"U1","name":"n","some_future_field":{"a":1}}"#);
        assert_eq!(u.id, "U1");
    }

    #[test]
    fn message_without_ts_deserialises_so_we_can_count_it_as_skipped() {
        let m: SlackMessage = serde_json::from_str(r#"{"type":"message","text":"x"}"#).unwrap();
        assert!(m.ts.is_none());
    }

    #[test]
    fn join_leave_detection() {
        assert!(is_join_leave(Some("channel_join")));
        assert!(is_join_leave(Some("group_leave")));
        assert!(!is_join_leave(Some("bot_message")));
        assert!(!is_join_leave(None));
    }

    #[test]
    fn deleted_files_are_recognised() {
        let f: SlackFile = serde_json::from_str(r#"{"id":"F1","mode":"file_deleted"}"#).unwrap();
        assert!(f.is_gone());
        let f: SlackFile = serde_json::from_str(r#"{"id":"F1","mode":"hosted"}"#).unwrap();
        assert!(!f.is_gone());
    }

    #[test]
    fn topic_and_purpose_unwrap_and_treat_blank_as_absent() {
        let c: SlackConversation =
            serde_json::from_str(r#"{"id":"C1","topic":{"value":"  "},"purpose":{"value":"why"}}"#)
                .unwrap();
        assert_eq!(c.topic_text(), None);
        assert_eq!(c.purpose_text(), Some("why".to_string()));
    }
}
