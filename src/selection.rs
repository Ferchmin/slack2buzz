//! Choosing which conversations to import.
//!
//! Selection is deliberately split in two. [`Filter`] is pure — it turns flags
//! plus an [`Inventory`] into a concrete list of Slack ids and is fully unit
//! tested. [`prompt`] is the interactive picker and does nothing but build a
//! `Filter`. Anything that decides what gets imported belongs in the pure half.
//!
//! Two rules that exist to stop quiet mistakes:
//!
//! - **An unknown selector is an error.** `--channels genral` (typo) fails
//!   loudly rather than importing nothing under that name. Silently skipping a
//!   misspelled channel is indistinguishable from success until the archive is
//!   already published.
//! - **Non-interactive runs never default to everything.** With no TTY and no
//!   explicit scope flag, `parse` refuses. Importing a workspace is not
//!   something to do by accident, and private channels and DMs are in scope for
//!   exactly the operators least likely to want them.

use std::collections::BTreeSet;

use anyhow::{bail, Result};

use crate::ir::ChannelKind;
use crate::probe::Inventory;

/// A pure description of which conversations to include.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    /// Include every conversation the export contains, of every kind.
    pub all: bool,
    /// Include all public channels.
    pub all_public: bool,
    /// Include all private channels.
    pub all_private: bool,
    /// Include all DMs and group DMs.
    pub all_dms: bool,
    /// Explicit picks, by channel name or Slack id.
    pub include: Vec<String>,
    /// Removed after everything else is resolved, so `--all --exclude x`
    /// reads the way it looks.
    pub exclude: Vec<String>,
    /// Include channels Slack has archived. Off by default: an archived
    /// channel is usually noise the operator has already decided against.
    pub include_archived: bool,
    /// Include conversations that have no messages in the export.
    pub include_empty: bool,
}

impl Filter {
    /// Whether the operator expressed any scope at all.
    pub fn is_empty(&self) -> bool {
        !self.all
            && !self.all_public
            && !self.all_private
            && !self.all_dms
            && self.include.is_empty()
    }

    pub fn everything() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }

    pub fn public_only() -> Self {
        Self {
            all_public: true,
            ..Self::default()
        }
    }
}

/// The outcome of applying a [`Filter`].
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// Slack ids to import, in inventory order.
    pub selected: Vec<String>,
    /// Slack ids present in the export but not selected.
    pub skipped: Vec<String>,
}

/// Apply a filter to an inventory.
///
/// Errors when a selector matches nothing, or when the result is empty — both
/// mean the operator asked for something they are not going to get.
pub fn resolve(inventory: &Inventory, filter: &Filter) -> Result<Resolved> {
    if filter.is_empty() {
        bail!(
            "no channels selected: pass --all, --all-public, --channels <names>, \
             or run without --no-input to choose interactively"
        );
    }

    let mut chosen: BTreeSet<String> = BTreeSet::new();

    for conversation in &inventory.conversations {
        let by_kind = filter.all
            || match conversation.kind {
                ChannelKind::Public => filter.all_public,
                ChannelKind::Private => filter.all_private,
                ChannelKind::Dm | ChannelKind::GroupDm => filter.all_dms,
            };
        if by_kind {
            chosen.insert(conversation.slack_id.clone());
        }
    }

    // Explicit picks bypass the archived/empty defaults — naming a channel is
    // a clearer signal of intent than a blanket flag.
    let mut explicit: BTreeSet<String> = BTreeSet::new();
    for selector in &filter.include {
        let matches = match_selector(inventory, selector);
        if matches.is_empty() {
            bail!(
                "no channel in this export matches \"{selector}\" — \
                 run `slack2buzz probe` to list what is available"
            );
        }
        explicit.extend(matches);
    }
    chosen.extend(explicit.iter().cloned());

    for selector in &filter.exclude {
        let matches = match_selector(inventory, selector);
        if matches.is_empty() {
            bail!("no channel in this export matches --exclude \"{selector}\"");
        }
        for id in matches {
            chosen.remove(&id);
        }
    }

    // Apply the archived/empty defaults, but never to an explicit pick.
    let selected: Vec<String> = inventory
        .conversations
        .iter()
        .filter(|c| chosen.contains(&c.slack_id))
        .filter(|c| {
            let named = explicit.contains(&c.slack_id);
            let archived_ok = filter.include_archived || !c.is_archived || named;
            let empty_ok = filter.include_empty || c.message_count > 0 || named;
            archived_ok && empty_ok
        })
        .map(|c| c.slack_id.clone())
        .collect();

    if selected.is_empty() {
        bail!("the selection matched no conversations with messages to import");
    }

    let skipped = inventory
        .conversations
        .iter()
        .filter(|c| !selected.contains(&c.slack_id))
        .map(|c| c.slack_id.clone())
        .collect();

    Ok(Resolved { selected, skipped })
}

/// Match one selector against the inventory, by exact Slack id or by
/// case-insensitive channel name. A leading `#` is accepted and ignored so
/// `--channels '#general'` works.
fn match_selector(inventory: &Inventory, selector: &str) -> Vec<String> {
    let needle = selector.trim().trim_start_matches('#');
    inventory
        .conversations
        .iter()
        .filter(|c| {
            c.slack_id == needle
                || c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(needle))
        })
        .map(|c| c.slack_id.clone())
        .collect()
}

/// Parse a comma-separated selector list, ignoring blanks so a trailing comma
/// is harmless.
pub fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read one selector per line from a file, ignoring blanks and `#` comments.
pub fn read_list_file(path: &std::path::Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

// ── interactive picker ───────────────────────────────────────────────────────

/// Presets offered before the per-channel list, so "select all" and "deselect
/// all" are single keystrokes rather than N toggles.
const PRESETS: &[&str] = &[
    "All public channels (recommended)",
    "Everything — public, private and DMs",
    "Nothing preselected — choose individually",
];

/// Ask the operator what to import.
///
/// Runs a two-step flow: a preset, then a checkbox list seeded from it. The
/// preset step is what gives select-all and deselect-all; the checkbox step is
/// where individual channels get toggled. Returns the resulting [`Filter`] so
/// the pure resolver still has the final say.
pub fn prompt(inventory: &Inventory) -> Result<Filter> {
    use dialoguer::{theme::ColorfulTheme, MultiSelect, Select};

    let theme = ColorfulTheme::default();

    let preset = Select::with_theme(&theme)
        .with_prompt("What should be imported?")
        .items(PRESETS)
        .default(0)
        .interact()?;

    let seed = match preset {
        0 => Filter::public_only(),
        1 => Filter::everything(),
        _ => Filter::default(),
    };

    // Everything with messages is offerable; the seed decides what starts
    // ticked. Archived and empty channels are shown but never pre-ticked, so
    // the operator sees they exist without having to opt out.
    let offerable: Vec<_> = inventory
        .conversations
        .iter()
        .filter(|c| c.message_count > 0)
        .collect();

    if offerable.is_empty() {
        bail!("this export contains no conversations with messages");
    }

    let preselected: BTreeSet<String> = match resolve(inventory, &seed) {
        Ok(r) => r.selected.into_iter().collect(),
        // "Nothing preselected" resolves to an error by design.
        Err(_) => BTreeSet::new(),
    };

    let items: Vec<String> = offerable.iter().map(|c| describe(c)).collect();
    let defaults: Vec<bool> = offerable
        .iter()
        .map(|c| preselected.contains(&c.slack_id))
        .collect();

    let picked = MultiSelect::with_theme(&theme)
        .with_prompt("space toggles one, enter confirms")
        .items(&items)
        .defaults(&defaults)
        .interact()?;

    if picked.is_empty() {
        bail!("nothing selected");
    }

    Ok(Filter {
        include: picked
            .into_iter()
            .map(|i| offerable[i].slack_id.clone())
            .collect(),
        // Explicit ids only — the picker's output is the whole truth.
        ..Filter::default()
    })
}

/// One line in the picker: what it is, how big, and when it ran.
fn describe(c: &crate::probe::ConversationSummary) -> String {
    let range = match (c.first_ts, c.last_ts) {
        (Some(f), Some(l)) => format!("{} → {}", crate::fmt::date(f), crate::fmt::date(l)),
        _ => "no messages".to_string(),
    };
    let flags = if c.is_archived { " [archived]" } else { "" };
    format!(
        "{:<24} {:<9} {:>6} msgs  {}{}",
        c.label(),
        c.kind.as_str(),
        c.message_count,
        range,
        flags
    )
}

#[cfg(test)]
mod tests {
    // A panic IS the failure report in a test; Buzz's CONTRIBUTING allows it.
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::export::Export;
    use std::path::Path;

    fn inventory() -> Inventory {
        let mut e = Export::open(Path::new("fixtures/basic-export")).unwrap();
        crate::probe::probe(&mut e, false).unwrap()
    }

    fn ids(r: &Resolved) -> Vec<&str> {
        r.selected.iter().map(String::as_str).collect()
    }

    #[test]
    fn empty_filter_is_refused_rather_than_defaulting_to_everything() {
        let inv = inventory();
        let err = resolve(&inv, &Filter::default()).unwrap_err().to_string();
        assert!(err.contains("no channels selected"), "{err}");
    }

    #[test]
    fn all_public_selects_only_public_channels_with_messages() {
        let inv = inventory();
        let r = resolve(&inv, &Filter::public_only()).unwrap();
        // `tumbleweed` is public but archived and empty, so it is left out.
        assert_eq!(ids(&r), vec!["C0GENERAL"]);
    }

    #[test]
    fn all_includes_private_and_dms() {
        let inv = inventory();
        let r = resolve(&inv, &Filter::everything()).unwrap();
        assert_eq!(ids(&r), vec!["C0GENERAL", "C0ENGPRIV", "D0ALICEBOB"]);
    }

    #[test]
    fn channels_can_be_named_by_name_case_insensitively_or_with_a_hash() {
        let inv = inventory();
        for selector in ["general", "GENERAL", "#general", "C0GENERAL"] {
            let f = Filter {
                include: vec![selector.to_string()],
                ..Filter::default()
            };
            assert_eq!(
                ids(&resolve(&inv, &f).unwrap()),
                vec!["C0GENERAL"],
                "{selector}"
            );
        }
    }

    #[test]
    fn a_typo_in_a_selector_is_an_error_not_a_silent_skip() {
        let inv = inventory();
        let f = Filter {
            include: vec!["genral".to_string()],
            ..Filter::default()
        };
        let err = resolve(&inv, &f).unwrap_err().to_string();
        assert!(err.contains("no channel in this export matches"), "{err}");
    }

    #[test]
    fn exclude_applies_after_the_kind_flags() {
        let inv = inventory();
        let f = Filter {
            all: true,
            exclude: vec!["general".to_string()],
            ..Filter::default()
        };
        assert_eq!(
            ids(&resolve(&inv, &f).unwrap()),
            vec!["C0ENGPRIV", "D0ALICEBOB"]
        );
    }

    #[test]
    fn a_typo_in_exclude_is_also_an_error() {
        let inv = inventory();
        let f = Filter {
            all: true,
            exclude: vec!["nope".to_string()],
            ..Filter::default()
        };
        assert!(resolve(&inv, &f).is_err());
    }

    #[test]
    fn naming_an_archived_empty_channel_explicitly_overrides_the_defaults() {
        let inv = inventory();
        let f = Filter {
            include: vec!["tumbleweed".to_string()],
            ..Filter::default()
        };
        assert_eq!(ids(&resolve(&inv, &f).unwrap()), vec!["C0EMPTY"]);
    }

    #[test]
    fn excluding_everything_selected_is_an_error() {
        let inv = inventory();
        let f = Filter {
            all_public: true,
            exclude: vec!["general".to_string()],
            ..Filter::default()
        };
        let err = resolve(&inv, &f).unwrap_err().to_string();
        assert!(err.contains("matched no conversations"), "{err}");
    }

    #[test]
    fn skipped_is_the_complement_of_selected() {
        let inv = inventory();
        let r = resolve(&inv, &Filter::public_only()).unwrap();
        assert_eq!(r.selected.len() + r.skipped.len(), inv.conversations.len());
        assert!(r.skipped.contains(&"C0ENGPRIV".to_string()));
    }

    #[test]
    fn selector_lists_tolerate_whitespace_and_trailing_commas() {
        assert_eq!(parse_list("a, b ,,c,"), vec!["a", "b", "c"]);
        assert_eq!(parse_list("").len(), 0);
    }
}
