//! `parse` — Slack export → `import.jsonl`.
//!
//! Pure with respect to the network and the clock: it reads files, writes one
//! file, and touches nothing else. That is what makes it testable against
//! golden files, and the golden files are where this project's notion of
//! correctness actually lives.
//!
//! Ordering is deliberate and stable so the output is diffable: the header,
//! then users, then per selected conversation the channel record followed by
//! its messages in ascending timestamp order, with each message's reactions
//! and file references immediately after it. A human scrolling `import.jsonl`
//! reads the conversation in the order it happened.

use std::collections::HashMap;
use std::io::Write;

use crate::error::{Error, Result};

use crate::export::Export;
use crate::ir::{self, ts_to_unix_secs, Record};
use crate::mrkdwn::{self, Resolver};
use crate::probe::Inventory;
use crate::slack::{self, SlackMessage};

/// Knobs that change what ends up in the IR.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Keep `channel_join` / `channel_leave` style messages.
    pub keep_joins: bool,
}

/// Parse the selected conversations into `writer`.
///
/// `selected` is a list of Slack conversation ids, normally straight from
/// [`crate::selection::resolve`]. `inventory` supplies the conversation
/// metadata and the user table, so `parse` never re-reads the manifests.
pub fn parse(
    export: &mut Export,
    inventory: &Inventory,
    selected: &[String],
    options: &Options,
    writer: &mut impl Write,
) -> Result<ir::Counts> {
    let resolver = build_resolver(inventory);
    let mut counts = ir::Counts::default();
    let mut body: Vec<Record> = Vec::new();

    // Users first: every later record refers to them.
    for user in &inventory.users {
        body.push(Record::User(ir::User {
            slack_id: user.id.clone(),
            name: user.name.clone(),
            display_name: user.best_display_name(),
            real_name: user
                .profile
                .as_ref()
                .and_then(|p| p.real_name.clone())
                .filter(|s| !s.trim().is_empty()),
            avatar_url: user.avatar_url(),
            timezone: user.tz.clone(),
            is_bot: user.is_bot,
            is_deleted: user.deleted,
        }));
        counts.users += 1;
    }

    if let Some(emoji) = export.read_json::<HashMap<String, String>>("emoji.json")? {
        // Sorted so the IR is byte-stable across runs; HashMap order is not.
        let mut names: Vec<_> = emoji.into_iter().collect();
        names.sort();
        for (name, url) in names {
            body.push(Record::Emoji(ir::Emoji { name, url }));
            counts.emoji += 1;
        }
    }

    for id in selected {
        let Some(summary) = inventory.conversations.iter().find(|c| &c.slack_id == id) else {
            return Err(Error::UnknownSelectedConversation { id: id.clone() });
        };

        body.push(Record::Channel(ir::Channel {
            slack_id: summary.slack_id.clone(),
            name: summary.name.clone(),
            kind: summary.kind,
            topic: None,
            purpose: None,
            creator: None,
            created: None,
            is_archived: summary.is_archived,
            members: summary.members.clone(),
        }));
        counts.channels += 1;

        let Some(dir) = &summary.dir else {
            continue;
        };
        parse_conversation(
            export,
            dir,
            summary,
            &resolver,
            options,
            &mut body,
            &mut counts,
        )?;
    }

    // The header carries the counts, so it can only be written once the body
    // is known. Buffering the body is the price; an import.jsonl is small
    // relative to the export it came from.
    let header = Record::Header(ir::Header {
        ir_version: ir::IR_VERSION,
        generator: format!("slack2buzz {}", env!("CARGO_PKG_VERSION")),
        source: "slack".to_string(),
        team_id: None,
        selected_channels: selected.to_vec(),
        skipped_channels: inventory
            .conversations
            .iter()
            .filter(|c| !selected.contains(&c.slack_id))
            .map(|c| c.slack_id.clone())
            .collect(),
        counts: counts.clone(),
    });

    write_record(writer, &header)?;
    for record in &body {
        write_record(writer, record)?;
    }
    writer
        .flush()
        .map_err(|e| Error::io("flushing the IR", e))?;

    Ok(counts)
}

fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
    serde_json::to_writer(&mut *writer, record).map_err(Error::Serialise)?;
    writer
        .write_all(b"\n")
        .map_err(|e| Error::io("writing the IR", e))?;
    Ok(())
}

/// Build the id → name maps that mrkdwn normalisation needs.
fn build_resolver(inventory: &Inventory) -> Resolver {
    let mut users = HashMap::new();
    for user in &inventory.users {
        users.insert(user.id.clone(), user.best_display_name());
    }
    let mut channels = HashMap::new();
    for conversation in &inventory.conversations {
        if let Some(name) = &conversation.name {
            channels.insert(conversation.slack_id.clone(), name.clone());
        }
    }
    Resolver { users, channels }
}

fn parse_conversation(
    export: &mut Export,
    dir: &str,
    summary: &crate::probe::ConversationSummary,
    resolver: &Resolver,
    options: &Options,
    body: &mut Vec<Record>,
    counts: &mut ir::Counts,
) -> Result<()> {
    // Collect the whole conversation before emitting so it can be sorted by
    // timestamp. Slack's day files are chronological individually, but a
    // thread reply can live in a later file than its root, and we want one
    // stable order regardless.
    let mut messages: Vec<SlackMessage> = Vec::new();

    for day in export.channel_day_files(dir)? {
        let Some(bytes) = export.read_bytes(&day)? else {
            continue;
        };
        match serde_json::from_slice::<Vec<SlackMessage>>(&bytes) {
            Ok(mut m) => messages.append(&mut m),
            Err(e) => {
                tracing::warn!(file = %day, error = %e, "skipping unparseable day file");
                counts.skipped_unparseable += 1;
            }
        }
    }

    messages.sort_by(|a, b| {
        // Slack ts strings are fixed-width within an era, but compare
        // numerically to be safe about width changes.
        let ka = a.ts.as_deref().and_then(ts_to_unix_secs).unwrap_or(0);
        let kb = b.ts.as_deref().and_then(ts_to_unix_secs).unwrap_or(0);
        ka.cmp(&kb).then_with(|| a.ts.cmp(&b.ts))
    });

    for message in messages {
        let handling = slack::handling(message.subtype.as_deref());
        if !options.keep_joins && handling == slack::Handling::JoinLeave {
            counts.dropped_joins += 1;
            continue;
        }
        // Imported anyway, but counted so the operator learns this build did
        // not recognise the subtype.
        if handling == slack::Handling::Unknown {
            if let Some(subtype) = &message.subtype {
                *counts.unknown_subtypes.entry(subtype.clone()).or_insert(0) += 1;
            }
        }

        let (Some(slack_ts), Some(created_at)) = (
            message.ts.clone(),
            message.ts.as_deref().and_then(ts_to_unix_secs),
        ) else {
            tracing::warn!(
                channel = %summary.slack_id,
                "message without a usable ts; skipping"
            );
            counts.skipped_unparseable += 1;
            continue;
        };

        let raw_text = message.text.clone().unwrap_or_default();
        let normalized = mrkdwn::normalize(&raw_text, resolver);

        let file_ids: Vec<String> = message.files.iter().filter_map(|f| f.id.clone()).collect();

        let ir_message = ir::Message {
            slack_ts: slack_ts.clone(),
            channel_slack_id: summary.slack_id.clone(),
            created_at,
            user_slack_id: message.user.clone(),
            bot_id: message.bot_id.clone(),
            author_override: message.username.clone(),
            text: normalized.text,
            raw_text,
            thread_ts: message.thread_ts.clone(),
            subtype: message.subtype.clone(),
            broadcast: handling == slack::Handling::Broadcast,
            edited_at: message
                .edited
                .as_ref()
                .and_then(|e| e.ts.as_deref())
                .and_then(ts_to_unix_secs),
            mentions: normalized.mentions,
            file_ids,
        };

        if ir_message.is_thread_reply() {
            counts.thread_replies += 1;
        }
        counts.messages += 1;
        body.push(Record::Message(ir_message));

        // Reactions immediately after their message, exploded per reactor and
        // ordered (emoji, reactor) so the output is stable.
        let mut reactions: Vec<(String, String)> = Vec::new();
        for reaction in &message.reactions {
            for user in &reaction.users {
                reactions.push((reaction.name.clone(), user.clone()));
            }
        }
        reactions.sort();
        for (name, user_slack_id) in reactions {
            body.push(Record::Reaction(ir::Reaction {
                channel_slack_id: summary.slack_id.clone(),
                target_slack_ts: slack_ts.clone(),
                name,
                user_slack_id,
            }));
            counts.reactions += 1;
        }

        for file in &message.files {
            let Some(id) = file.id.clone() else {
                continue;
            };
            body.push(Record::File(ir::FileRef {
                slack_file_id: id,
                channel_slack_id: summary.slack_id.clone(),
                message_slack_ts: slack_ts.clone(),
                name: file.name.clone(),
                mimetype: file.mimetype.clone(),
                size: file.size,
                url_private: file.url_private.clone(),
                permalink: file.permalink.clone(),
                is_deleted: file.is_gone(),
                is_external: file.is_external,
            }));
            counts.files += 1;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{self, Filter};
    use std::path::Path;

    fn run(filter: &Filter, options: &Options) -> (String, ir::Counts) {
        let mut export = Export::open(Path::new("fixtures/basic-export")).unwrap();
        let inventory = crate::probe::probe(&mut export, options.keep_joins).unwrap();
        let resolved = selection::resolve(&inventory, filter).unwrap();
        let mut out = Vec::new();
        let counts = parse(
            &mut export,
            &inventory,
            &resolved.selected,
            options,
            &mut out,
        )
        .unwrap();
        (String::from_utf8(out).unwrap(), counts)
    }

    fn records(jsonl: &str) -> Vec<Record> {
        jsonl
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is a Record"))
            .collect()
    }

    #[test]
    fn first_line_is_always_the_header() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let first = records(&jsonl).into_iter().next().unwrap();
        match first {
            Record::Header(h) => {
                assert_eq!(h.ir_version, ir::IR_VERSION);
                assert_eq!(h.source, "slack");
                assert_eq!(h.selected_channels, vec!["C0GENERAL"]);
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    #[test]
    fn header_counts_match_the_records_that_follow() {
        let (jsonl, counts) = run(&Filter::everything(), &Options::default());
        let recs = records(&jsonl);
        let actual_messages = recs
            .iter()
            .filter(|r| matches!(r, Record::Message(_)))
            .count();
        let actual_reactions = recs
            .iter()
            .filter(|r| matches!(r, Record::Reaction(_)))
            .count();
        assert_eq!(counts.messages, actual_messages);
        assert_eq!(counts.reactions, actual_reactions);

        match recs.into_iter().next().unwrap() {
            Record::Header(h) => {
                assert_eq!(h.counts.messages, actual_messages);
                assert_eq!(h.counts.reactions, actual_reactions);
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    #[test]
    fn every_line_is_valid_json() {
        let (jsonl, _) = run(&Filter::everything(), &Options::default());
        assert!(
            jsonl.ends_with('\n'),
            "trailing newline keeps appends clean"
        );
        for line in jsonl.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("valid JSON per line");
        }
    }

    #[test]
    fn unselected_channels_contribute_no_records() {
        // A raw substring check would false-positive on the header's
        // `skipped_channels` list and on a message that mentions
        // `<#C0ENGPRIV|eng-private>`, so inspect the records themselves.
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        for record in records(&jsonl) {
            let channel = match record {
                Record::Channel(c) => c.slack_id,
                Record::Message(m) => m.channel_slack_id,
                Record::Reaction(r) => r.channel_slack_id,
                Record::File(f) => f.channel_slack_id,
                _ => continue,
            };
            assert_eq!(
                channel, "C0GENERAL",
                "only the selected channel may contribute records"
            );
        }

        match records(&jsonl).into_iter().next().unwrap() {
            Record::Header(h) => {
                assert!(h.skipped_channels.contains(&"C0ENGPRIV".to_string()));
                assert!(h.skipped_channels.contains(&"D0ALICEBOB".to_string()));
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    #[test]
    fn messages_are_ordered_by_timestamp_within_a_channel() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let times: Vec<i64> = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) => Some(m.created_at),
                _ => None,
            })
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    }

    #[test]
    fn joins_are_dropped_by_default_and_kept_on_request() {
        let (dropped, counts_dropped) = run(&Filter::public_only(), &Options::default());
        assert!(!dropped.contains("channel_join"));
        assert!(counts_dropped.dropped_joins > 0);

        let (kept, counts_kept) = run(&Filter::public_only(), &Options { keep_joins: true });
        assert!(kept.contains("channel_join"));
        assert_eq!(counts_kept.dropped_joins, 0);
        assert_eq!(
            counts_kept.messages,
            counts_dropped.messages + counts_dropped.dropped_joins
        );
    }

    #[test]
    fn a_message_without_a_ts_is_counted_as_skipped_not_imported() {
        let (_, counts) = run(&Filter::public_only(), &Options::default());
        assert_eq!(
            counts.skipped_unparseable, 1,
            "the fixture contains exactly one ts-less message"
        );
    }

    #[test]
    fn reactions_are_exploded_one_per_reactor() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let eyes: Vec<_> = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Reaction(x) if x.name == "eyes" => Some(x.user_slack_id),
                _ => None,
            })
            .collect();
        assert_eq!(eyes, vec!["U024BE7LH", "U0BOB"], "sorted, one per reactor");
    }

    #[test]
    fn thread_replies_are_marked_and_counted() {
        let (jsonl, counts) = run(&Filter::public_only(), &Options::default());
        let replies: Vec<_> = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.is_thread_reply() => Some(m.slack_ts),
                _ => None,
            })
            .collect();
        // Three: two ordinary replies plus the broadcast, which is a reply too.
        assert_eq!(replies.len(), 3);
        assert_eq!(counts.thread_replies, 3);
    }

    /// A `thread_broadcast` reply went to the channel as well as the thread.
    /// Losing that flag would silently downgrade it to an ordinary reply.
    #[test]
    fn a_broadcast_reply_is_flagged_and_still_a_reply() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let broadcast = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.broadcast => Some(m),
                _ => None,
            })
            .next()
            .expect("the fixture has a thread_broadcast message");

        assert_eq!(broadcast.subtype.as_deref(), Some("thread_broadcast"));
        assert!(
            broadcast.is_thread_reply(),
            "a broadcast is still a reply and must attach to its root"
        );
        assert_eq!(broadcast.thread_ts.as_deref(), Some("1709545500.000500"));
    }

    #[test]
    fn ordinary_messages_are_not_flagged_as_broadcast() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let flagged = records(&jsonl)
            .into_iter()
            .filter(|r| matches!(r, Record::Message(m) if m.broadcast))
            .count();
        assert_eq!(flagged, 1, "exactly the one broadcast in the fixture");
    }

    /// Unrecognised subtypes are imported, but must be counted so the operator
    /// can find out. Slack keeps adding subtypes; silence is the failure mode.
    #[test]
    fn an_unrecognised_subtype_is_imported_and_counted() {
        let (jsonl, counts) = run(&Filter::public_only(), &Options::default());
        assert_eq!(
            counts.unknown_subtypes.get("some_future_slack_thing"),
            Some(&1)
        );

        let imported = records(&jsonl).into_iter().any(|r| {
            matches!(r, Record::Message(m)
                if m.subtype.as_deref() == Some("some_future_slack_thing"))
        });
        assert!(imported, "counted, but still imported — not dropped");
    }

    #[test]
    fn recognised_subtypes_are_not_counted_as_unknown() {
        let (_, counts) = run(&Filter::public_only(), &Options::default());
        for known in ["bot_message", "channel_topic", "thread_broadcast"] {
            assert!(
                !counts.unknown_subtypes.contains_key(known),
                "{known} is recognised and must not be reported"
            );
        }
    }

    #[test]
    fn thread_root_is_not_counted_as_a_reply() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let root = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.slack_ts == "1709545500.000500" => Some(m),
                _ => None,
            })
            .next()
            .unwrap();
        assert!(root.is_thread_root());
        assert!(!root.is_thread_reply());
    }

    #[test]
    fn raw_text_is_preserved_alongside_the_normalised_text() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let m = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.slack_ts == "1709545400.000400" => Some(m),
                _ => None,
            })
            .next()
            .unwrap();
        assert!(m.raw_text.contains("<@U024BE7LH>"), "raw kept verbatim");
        // `display_name` is empty for this user, so Slack's own preference
        // order falls through to `real_name`.
        assert!(
            m.text.contains("@Paweł Zieliński"),
            "normalised text resolved: {}",
            m.text
        );
        assert!(m.text.contains("**deploy**"), "mrkdwn converted");
        assert!(
            m.text.contains("#eng-private"),
            "channel ref uses its label"
        );
        assert!(
            m.text.contains("[build 42](https://ci.test/build/42)"),
            "labelled link became Markdown"
        );
        assert!(m.text.contains("@here"), "broadcast keyword");
        assert!(m.text.contains("5 & rising"), "entities decoded");
        assert!(
            m.text.contains("if (a < b)"),
            "escaped angle brackets decoded"
        );
        assert_eq!(m.mentions, vec!["U024BE7LH"]);
    }

    #[test]
    fn deleted_files_are_flagged_rather_than_dropped() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let files: Vec<_> = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::File(f) => Some((f.slack_file_id, f.is_deleted)),
                _ => None,
            })
            .collect();
        assert_eq!(
            files,
            vec![("F0NOTES".to_string(), false), ("F0GONE".to_string(), true)]
        );
    }

    #[test]
    fn bot_messages_keep_their_own_display_name() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let bot = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.bot_id.is_some() => Some(m),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(bot.author_override.as_deref(), Some("CI"));
        assert_eq!(bot.user_slack_id, None);
    }

    #[test]
    fn edits_record_when_they_happened() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let edited = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::Message(m) if m.edited_at.is_some() => Some(m),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(edited.edited_at, Some(1_709_632_000));
    }

    #[test]
    fn deleted_users_are_still_emitted_so_their_messages_are_not_orphaned() {
        let (jsonl, _) = run(&Filter::public_only(), &Options::default());
        let gone = records(&jsonl)
            .into_iter()
            .filter_map(|r| match r {
                Record::User(u) if u.slack_id == "U0GONE" => Some(u),
                _ => None,
            })
            .next()
            .unwrap();
        assert!(gone.is_deleted);
    }

    #[test]
    fn output_is_byte_stable_across_runs() {
        let (a, _) = run(&Filter::everything(), &Options::default());
        let (b, _) = run(&Filter::everything(), &Options::default());
        assert_eq!(
            a, b,
            "emoji and reaction ordering must not depend on hashing"
        );
    }
}
