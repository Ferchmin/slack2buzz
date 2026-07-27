//! `probe` — report what an export actually contains, before anything is sent.
//!
//! This is the Slack-tier detection. A workspace on the free plan exports only
//! public channels; paid plans can include private channels and DMs, and an
//! operator often does not know which they asked for until they look. We derive
//! it from the export and never ask, because the export is the ground truth and
//! the operator's belief frequently is not.
//!
//! Probing reads message files to count them but keeps nothing but tallies, so
//! it stays cheap on a multi-gigabyte export.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::export::Export;
use crate::ir::{ts_to_unix_secs, ChannelKind};
use crate::slack::{is_join_leave, SlackConversation, SlackMessage, SlackUser};

/// What tiers of conversation the export turned out to contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
    /// Public channels only — the free-plan shape.
    PublicOnly,
    /// Public plus private channels.
    WithPrivate,
    /// Includes direct messages.
    WithDms,
}

impl ExportScope {
    pub fn describe(self) -> &'static str {
        match self {
            Self::PublicOnly => "public channels only",
            Self::WithPrivate => "public and private channels",
            Self::WithDms => "public channels, private channels and direct messages",
        }
    }
}

/// Per-conversation tallies.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub slack_id: String,
    pub name: Option<String>,
    pub kind: ChannelKind,
    /// Directory inside the export holding this conversation's day files.
    /// `None` when the manifest lists a conversation the export has no
    /// messages for — common for channels the exporter had no access to.
    pub dir: Option<String>,
    pub is_archived: bool,
    pub members: Vec<String>,
    /// Messages that would be imported, after join/leave filtering.
    pub message_count: usize,
    pub thread_reply_count: usize,
    pub reaction_count: usize,
    pub file_count: usize,
    pub dropped_joins: usize,
    pub skipped_unparseable: usize,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

impl ConversationSummary {
    /// Human label: channel name, or the Slack id for DMs which have none.
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => self.slack_id.clone(),
        }
    }
}

/// The full result of a probe.
#[derive(Debug)]
pub struct Inventory {
    pub scope: ExportScope,
    pub users: Vec<SlackUser>,
    pub conversations: Vec<ConversationSummary>,
    pub emoji_count: usize,
    /// Things the operator should know that are not errors: manifests present
    /// but empty, message directories with no manifest entry, and so on.
    pub warnings: Vec<String>,
}

impl Inventory {
    pub fn total_messages(&self) -> usize {
        self.conversations.iter().map(|c| c.message_count).sum()
    }
    pub fn total_reactions(&self) -> usize {
        self.conversations.iter().map(|c| c.reaction_count).sum()
    }
    pub fn total_files(&self) -> usize {
        self.conversations.iter().map(|c| c.file_count).sum()
    }
    pub fn total_thread_replies(&self) -> usize {
        self.conversations
            .iter()
            .map(|c| c.thread_reply_count)
            .sum()
    }

    /// Conversations of a given kind.
    pub fn of_kind(&self, kind: ChannelKind) -> impl Iterator<Item = &ConversationSummary> {
        self.conversations.iter().filter(move |c| c.kind == kind)
    }
}

/// Manifest file → conversation kind.
const MANIFESTS: &[(&str, ChannelKind)] = &[
    ("channels.json", ChannelKind::Public),
    ("groups.json", ChannelKind::Private),
    ("mpims.json", ChannelKind::GroupDm),
    ("dms.json", ChannelKind::Dm),
];

/// Walk the export and tally everything.
///
/// `keep_joins` must match what `parse` will be told, or the counts shown to
/// the operator will not be the counts they get.
pub fn probe(export: &mut Export, keep_joins: bool) -> Result<Inventory> {
    let mut warnings = Vec::new();

    let users: Vec<SlackUser> = export
        .read_json::<Vec<SlackUser>>("users.json")
        .context("reading users.json")?
        .unwrap_or_else(|| {
            warnings.push(
                "users.json is missing — messages will be attributed to raw Slack ids".to_string(),
            );
            Vec::new()
        });

    // Which conversation directories exist on disk, so we can flag manifest
    // entries with no messages and directories with no manifest entry.
    let dirs = export.top_level_dirs()?;
    let mut unclaimed: BTreeMap<String, ()> = dirs
        .iter()
        .filter(|d| !is_metadata_dir(d))
        .map(|d| (d.clone(), ()))
        .collect();

    let mut conversations = Vec::new();
    let mut present_kinds = Vec::new();

    for (manifest, kind) in MANIFESTS {
        let Some(entries) = export
            .read_json::<Vec<SlackConversation>>(manifest)
            .with_context(|| format!("reading {manifest}"))?
        else {
            continue;
        };
        if entries.is_empty() {
            warnings.push(format!("{manifest} is present but empty"));
            continue;
        }
        present_kinds.push(*kind);

        for entry in entries {
            // Slack names public/private channel directories after the
            // channel; DM directories after the conversation id.
            let candidate = entry.name.clone().unwrap_or_else(|| entry.slack_dir());
            let dir = if dirs.contains(&candidate) {
                unclaimed.remove(&candidate);
                Some(candidate)
            } else if dirs.contains(&entry.id) {
                unclaimed.remove(&entry.id);
                Some(entry.id.clone())
            } else {
                None
            };

            let mut summary = ConversationSummary {
                slack_id: entry.id.clone(),
                name: entry.name.clone(),
                kind: *kind,
                dir: dir.clone(),
                is_archived: entry.is_archived,
                members: entry.members.clone(),
                message_count: 0,
                thread_reply_count: 0,
                reaction_count: 0,
                file_count: 0,
                dropped_joins: 0,
                skipped_unparseable: 0,
                first_ts: None,
                last_ts: None,
            };

            if let Some(dir) = &dir {
                tally_conversation(export, dir, keep_joins, &mut summary)?;
            } else {
                warnings.push(format!(
                    "{} \"{}\" is listed in {manifest} but has no messages in the export",
                    kind.as_str(),
                    summary.label()
                ));
            }

            conversations.push(summary);
        }
    }

    for dir in unclaimed.keys() {
        warnings.push(format!(
            "directory \"{dir}\" holds messages but is not listed in any manifest — it will not be imported"
        ));
    }

    let emoji_count = export
        .read_json::<BTreeMap<String, String>>("emoji.json")?
        .map(|m| m.len())
        .unwrap_or(0);

    let scope = if present_kinds
        .iter()
        .any(|k| matches!(k, ChannelKind::Dm | ChannelKind::GroupDm))
    {
        ExportScope::WithDms
    } else if present_kinds.contains(&ChannelKind::Private) {
        ExportScope::WithPrivate
    } else {
        ExportScope::PublicOnly
    };

    Ok(Inventory {
        scope,
        users,
        conversations,
        emoji_count,
        warnings,
    })
}

impl SlackConversation {
    /// Directory Slack would have used for this conversation when it has no
    /// name (DMs and, in some exports, MPIMs).
    fn slack_dir(&self) -> String {
        self.id.clone()
    }
}

/// Directories at the export root that are not conversations.
fn is_metadata_dir(name: &str) -> bool {
    matches!(name, "canvases" | "lists" | "huddle_transcripts" | "files")
}

fn tally_conversation(
    export: &mut Export,
    dir: &str,
    keep_joins: bool,
    summary: &mut ConversationSummary,
) -> Result<()> {
    for day in export.channel_day_files(dir)? {
        let Some(bytes) = export.read_bytes(&day)? else {
            continue;
        };
        // A single unreadable day file should not abort the whole probe; it is
        // a fidelity problem to report, not a crash.
        let messages: Vec<SlackMessage> = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(file = %day, error = %e, "unparseable day file");
                summary.skipped_unparseable += 1;
                continue;
            }
        };

        for message in messages {
            if !keep_joins && is_join_leave(message.subtype.as_deref()) {
                summary.dropped_joins += 1;
                continue;
            }
            let Some(ts) = message.ts.as_deref().and_then(ts_to_unix_secs) else {
                summary.skipped_unparseable += 1;
                continue;
            };

            summary.message_count += 1;
            summary.first_ts = Some(summary.first_ts.map_or(ts, |f| f.min(ts)));
            summary.last_ts = Some(summary.last_ts.map_or(ts, |l| l.max(ts)));

            if message
                .thread_ts
                .as_deref()
                .is_some_and(|t| Some(t) != message.ts.as_deref())
            {
                summary.thread_reply_count += 1;
            }
            summary.reaction_count += message
                .reactions
                .iter()
                .map(|r| r.users.len())
                .sum::<usize>();
            summary.file_count += message.files.len();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // A panic IS the failure report in a test; Buzz's CONTRIBUTING allows it.
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::Path;

    fn fixture() -> Export {
        Export::open(Path::new("fixtures/basic-export")).unwrap()
    }

    #[test]
    fn fixture_export_scope_includes_dms() {
        let mut e = fixture();
        let inv = probe(&mut e, false).unwrap();
        assert_eq!(inv.scope, ExportScope::WithDms);
    }

    #[test]
    fn fixture_finds_all_three_conversations() {
        let mut e = fixture();
        let inv = probe(&mut e, false).unwrap();
        assert_eq!(
            inv.conversations.len(),
            4,
            "general, tumbleweed, eng-private, the DM"
        );
        assert_eq!(inv.of_kind(ChannelKind::Public).count(), 2);
        assert_eq!(inv.of_kind(ChannelKind::Private).count(), 1);
        assert_eq!(inv.of_kind(ChannelKind::Dm).count(), 1);
    }

    #[test]
    fn join_messages_are_dropped_by_default_and_counted() {
        let mut e = fixture();
        let inv = probe(&mut e, false).unwrap();
        let general = inv
            .conversations
            .iter()
            .find(|c| c.name.as_deref() == Some("general"))
            .unwrap();
        assert!(
            general.dropped_joins > 0,
            "fixture should exercise join filtering"
        );

        let mut e = fixture();
        let kept = probe(&mut e, true).unwrap();
        let general_kept = kept
            .conversations
            .iter()
            .find(|c| c.name.as_deref() == Some("general"))
            .unwrap();
        assert_eq!(
            general_kept.message_count,
            general.message_count + general.dropped_joins,
            "--keep-joins should add exactly the dropped joins back"
        );
    }

    #[test]
    fn date_range_is_derived_from_message_timestamps() {
        let mut e = fixture();
        let inv = probe(&mut e, false).unwrap();
        let general = inv
            .conversations
            .iter()
            .find(|c| c.name.as_deref() == Some("general"))
            .unwrap();
        let (first, last) = (general.first_ts.unwrap(), general.last_ts.unwrap());
        assert!(first <= last);
    }

    #[test]
    fn reactions_are_counted_per_reactor_not_per_emoji() {
        let mut e = fixture();
        let inv = probe(&mut e, false).unwrap();
        // The fixture has one emoji reacted to by two people.
        assert!(inv.total_reactions() >= 2);
    }
}
