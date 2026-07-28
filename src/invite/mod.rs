//! `invite` — work out who to invite, then DM each of them a join link.
//!
//! Split the same way as the rest of the tool: this module is the **pure**
//! planning half and is fully unit tested. The network half lives in
//! [`slack`] and [`buzz`] behind traits, so the planner can be exercised end to
//! end without a token.
//!
//! # Planning needs no Slack API
//!
//! The obvious design is `conversations.members` → `users.list` → invite. It
//! turns out neither call is needed to *plan*: the export already contains
//! channel membership and the user table, and `chat.postMessage` accepts a user
//! id directly as its `channel`, opening the DM implicitly. So no email address
//! is required and no directory lookup happens — a Slack token is needed only to
//! actually send.
//!
//! The cost of that is staleness: the export is a snapshot, so someone who left
//! after it was taken still looks active. The export's own `deleted` flag covers
//! anyone deactivated *before* the export, which is the common case. A
//! `--verify-directory` pass against `users.list` would close the gap and is not
//! built.
//!
//! # Why this stage is the careful one
//!
//! Every other stage is recoverable. This one messages real colleagues, once,
//! and cannot be unsent. Hence: dry run is the default, the candidate set is
//! derived from the channels actually imported rather than the whole workspace,
//! and every exclusion is reported rather than silently applied.

pub mod buzz;
pub mod slack;

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};

use crate::ir::Record;

/// Someone who could be invited.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub slack_id: String,
    /// Slack handle (`alice`).
    pub name: String,
    /// Best human name, used in the DM greeting.
    pub display_name: String,
    pub is_bot: bool,
    pub is_deleted: bool,
    /// Channels in the import this person is a member of.
    pub channels: Vec<String>,
    /// Messages this person authored in the imported channels.
    pub message_count: usize,
}

impl Person {
    /// Whether this person could ever be invited, regardless of selection.
    ///
    /// Bots have no human to onboard and deactivated accounts cannot join, so
    /// neither is ever a candidate. Both are reported, not silently dropped.
    fn hard_excluded(&self) -> Option<Exclusion> {
        if self.is_bot {
            Some(Exclusion::Bot)
        } else if self.is_deleted {
            Some(Exclusion::Deactivated)
        } else {
            None
        }
    }
}

/// Why someone is not being invited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    Bot,
    Deactivated,
    /// Not a member of any imported channel.
    NotInImportedChannels,
    /// Never posted, and the operator asked for posters only.
    NeverPosted,
    /// Named in `--exclude-users`.
    ExcludedByOperator,
    /// Not picked in the interactive list, or not named in `--users`.
    NotSelected,
    /// The ledger says this person was already invited.
    AlreadyInvited,
}

impl Exclusion {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Bot => "is a bot",
            Self::Deactivated => "account is deactivated",
            Self::NotInImportedChannels => "not in any imported channel",
            Self::NeverPosted => "never posted",
            Self::ExcludedByOperator => "excluded by operator",
            Self::NotSelected => "not selected",
            Self::AlreadyInvited => "already invited (ledger)",
        }
    }
}

/// Which people to consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Members of the channels that were imported. The default: nobody gets a
    /// DM because of a channel the operator chose not to import.
    #[default]
    ImportedChannelMembers,
    /// Everyone in the export's user table.
    Everyone,
    /// Only people who actually authored a message in the imported channels.
    /// Smallest set; good for a pilot, but omits lurkers.
    PostersOnly,
}

/// What the operator asked for.
#[derive(Debug, Clone, Default)]
pub struct PersonFilter {
    pub scope: Scope,
    /// Explicit picks by handle or Slack id. Non-empty means *only* these,
    /// regardless of `scope`.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// A person who will be invited.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipient {
    pub person: Person,
}

/// The full plan, including who is being left out and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub recipients: Vec<Recipient>,
    pub excluded: Vec<(Person, Exclusion)>,
}

impl Plan {
    pub fn len(&self) -> usize {
        self.recipients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recipients.is_empty()
    }

    /// Counts per exclusion reason, for the summary line.
    pub fn exclusion_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for (_, reason) in &self.excluded {
            *counts.entry(reason.describe()).or_insert(0) += 1;
        }
        counts
    }
}

/// Build the candidate set from a parsed IR.
///
/// Channel membership comes from the `channel` records' `members`, and message
/// counts from the `message` records, so both reflect exactly what was imported.
pub fn candidates(records: &[Record]) -> Vec<Person> {
    let mut users: BTreeMap<String, Person> = BTreeMap::new();
    let mut imported_channels: BTreeSet<String> = BTreeSet::new();

    for record in records {
        if let Record::User(u) = record {
            users.insert(
                u.slack_id.clone(),
                Person {
                    slack_id: u.slack_id.clone(),
                    name: u.name.clone(),
                    display_name: u.display_name.clone(),
                    is_bot: u.is_bot,
                    is_deleted: u.is_deleted,
                    channels: Vec::new(),
                    message_count: 0,
                },
            );
        }
    }

    for record in records {
        match record {
            Record::Channel(c) => {
                imported_channels.insert(c.slack_id.clone());
                for member in &c.members {
                    if let Some(person) = users.get_mut(member) {
                        person.channels.push(c.slack_id.clone());
                    }
                }
            }
            Record::Message(m) => {
                if let Some(id) = &m.user_slack_id {
                    if let Some(person) = users.get_mut(id) {
                        person.message_count += 1;
                    }
                }
            }
            _ => {}
        }
    }

    // Stable order: most active first, then by id so ties are deterministic.
    let mut people: Vec<Person> = users.into_values().collect();
    people.sort_by(|a, b| {
        b.message_count
            .cmp(&a.message_count)
            .then_with(|| a.slack_id.cmp(&b.slack_id))
    });
    people
}

/// Apply a filter to the candidates.
///
/// `already_invited` is consulted so a resumed run does not re-DM anyone; pass
/// an empty set for a fresh plan.
pub fn plan(
    people: &[Person],
    filter: &PersonFilter,
    already_invited: &BTreeSet<String>,
) -> Result<Plan> {
    // Resolve explicit selectors first so a typo fails before anything else.
    let explicit = resolve_selectors(people, &filter.include, "--users")?;
    let excluded_ids = resolve_selectors(people, &filter.exclude, "--exclude-users")?;

    let mut recipients = Vec::new();
    let mut excluded = Vec::new();

    for person in people {
        // Hard exclusions win over everything, including an explicit pick:
        // naming a bot does not make it invitable.
        if let Some(reason) = person.hard_excluded() {
            excluded.push((person.clone(), reason));
            continue;
        }

        if excluded_ids.contains(&person.slack_id) {
            excluded.push((person.clone(), Exclusion::ExcludedByOperator));
            continue;
        }

        let wanted = if explicit.is_empty() {
            match filter.scope {
                Scope::Everyone => true,
                Scope::ImportedChannelMembers => !person.channels.is_empty(),
                Scope::PostersOnly => person.message_count > 0,
            }
        } else {
            explicit.contains(&person.slack_id)
        };

        if !wanted {
            let reason = if !explicit.is_empty() {
                Exclusion::NotSelected
            } else {
                match filter.scope {
                    Scope::PostersOnly => Exclusion::NeverPosted,
                    _ => Exclusion::NotInImportedChannels,
                }
            };
            excluded.push((person.clone(), reason));
            continue;
        }

        if already_invited.contains(&person.slack_id) {
            excluded.push((person.clone(), Exclusion::AlreadyInvited));
            continue;
        }

        recipients.push(Recipient {
            person: person.clone(),
        });
    }

    Ok(Plan {
        recipients,
        excluded,
    })
}

/// Match selectors against people, erroring on any that match nothing.
///
/// Same rule as channel selection: a typo must fail loudly. Silently skipping a
/// misspelled handle means someone never gets invited and nobody notices.
fn resolve_selectors(
    people: &[Person],
    selectors: &[String],
    flag: &str,
) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for selector in selectors {
        let needle = selector.trim().trim_start_matches('@');
        let matches: Vec<&Person> = people
            .iter()
            .filter(|p| {
                p.slack_id == needle
                    || p.name.eq_ignore_ascii_case(needle)
                    || p.display_name.eq_ignore_ascii_case(needle)
            })
            .collect();

        match matches.len() {
            0 => {
                return Err(Error::UnknownPerson {
                    flag: flag.to_string(),
                    selector: selector.clone(),
                })
            }
            _ => out.extend(matches.into_iter().map(|p| p.slack_id.clone())),
        }
    }
    Ok(out)
}

/// Load an `import.jsonl` into records.
pub fn load_ir(path: &std::path::Path) -> Result<Vec<Record>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(line).map_err(|e| Error::MalformedIr {
            path: path.display().to_string(),
            line: i + 1,
            source: e,
        })?;
        records.push(record);
    }
    Ok(records)
}

/// The DM body. Deliberately plain and short: it is going to real people who
/// did not ask for it, and it has to say what it is in the first line.
pub fn compose_dm(person: &Person, community: &str, invite_url: &str, expires: &str) -> String {
    format!(
        "Hi {name} — we're moving our Slack history to {community}, a self-hosted \
Buzz community.\n\n\
Your invite: {invite_url}\n\
This link expires {expires}.\n\n\
Note: the imported history is an *archive*. Old messages appear under \
placeholder \"[archive]\" accounts, not real identities — including yours. \
Joining gives you your own account going forward; it does not give you \
ownership of the archived messages.\n\n\
Anyone with this link can join, so please don't forward it.",
        name = person.display_name,
    )
}

/// Presets offered before the per-person list, so "everyone" and "nobody
/// preselected" are single keystrokes rather than N toggles.
const PRESETS: &[&str] = &[
    "Members of the imported channels (recommended)",
    "Only people who actually posted",
    "Everyone in the export",
    "Nobody preselected — choose individually",
];

/// Ask the operator who to invite.
///
/// Same two-step shape as channel selection: a preset, then a checkbox list
/// seeded from it. The preset step is what gives select-all and deselect-all;
/// the checkbox step is where individuals get toggled. Returns a
/// [`PersonFilter`] so the pure planner still has the final say.
pub fn prompt(people: &[Person]) -> Result<PersonFilter> {
    use dialoguer::{theme::ColorfulTheme, MultiSelect, Select};

    let theme = ColorfulTheme::default();

    let preset = Select::with_theme(&theme)
        .with_prompt("Who should be invited?")
        .items(PRESETS)
        .default(0)
        .interact()?;

    let seed = match preset {
        0 => Scope::ImportedChannelMembers,
        1 => Scope::PostersOnly,
        2 => Scope::Everyone,
        _ => Scope::ImportedChannelMembers,
    };

    // Only offer people who could actually be invited; bots and deactivated
    // accounts are shown in the exclusion report instead of cluttering a list
    // where ticking them would do nothing.
    let offerable: Vec<&Person> = people
        .iter()
        .filter(|p| p.hard_excluded().is_none())
        .collect();
    if offerable.is_empty() {
        return Err(Error::NoInvitablePeople);
    }

    let preselected: BTreeSet<String> = if preset == 3 {
        BTreeSet::new()
    } else {
        plan(
            people,
            &PersonFilter {
                scope: seed,
                ..PersonFilter::default()
            },
            &BTreeSet::new(),
        )?
        .recipients
        .into_iter()
        .map(|r| r.person.slack_id)
        .collect()
    };

    let items: Vec<String> = offerable.iter().map(|p| describe(p)).collect();
    let defaults: Vec<bool> = offerable
        .iter()
        .map(|p| preselected.contains(&p.slack_id))
        .collect();

    let picked = MultiSelect::with_theme(&theme)
        .with_prompt("space toggles one, enter confirms")
        .items(&items)
        .defaults(&defaults)
        .interact()?;

    if picked.is_empty() {
        return Err(Error::NobodySelected);
    }

    Ok(PersonFilter {
        include: picked
            .into_iter()
            .map(|i| offerable[i].slack_id.clone())
            .collect(),
        ..PersonFilter::default()
    })
}

/// One line in the picker: who they are, how active, and how much of the
/// import they appear in.
fn describe(p: &Person) -> String {
    format!(
        "{:<24} @{:<18} {:>5} msgs  {} channels",
        p.display_name,
        p.name,
        p.message_count,
        p.channels.len()
    )
}

/// What actually happened.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub sent: Vec<String>,
    /// `(slack_user_id, error)` — reported per person, never collapsed into a
    /// single "the import failed".
    pub failed: Vec<(String, String)>,
}

impl Outcome {
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Mint an invite for each recipient and DM it to them.
///
/// The per-recipient order is deliberate:
///
/// 1. **Mint.** A failure here means no DM is sent, so nobody receives a
///    message promising a link that does not exist.
/// 2. **Record `Minted`, with the code.** Written *before* the DM so a crash in
///    between leaves the code in the ledger rather than losing it silently.
/// 3. **Send.**
/// 4. **Record `Sent`.** Only now is the person considered done, so a crash
///    before this point retries them — sending twice is bad, but never sending
///    is worse, and the ledger's `Sent` state is what makes the retry safe.
///
/// One recipient's failure never aborts the rest: a rate limit on person 40 of
/// 200 must not strand the remaining 160.
pub fn execute(
    plan: &Plan,
    community: &str,
    ttl_secs: u64,
    minter: &mut impl buzz::Minter,
    messenger: &mut impl slack::Messenger,
    ledger: &crate::ledger::Ledger,
    now: i64,
) -> Result<Outcome> {
    use crate::ledger::State;

    // Fail before the first DM, not after the fortieth.
    let role = minter.role()?;
    if !role.can_mint() {
        return Err(Error::InsufficientRole {
            role: format!("{role:?}").to_lowercase(),
            community: community.to_string(),
        });
    }

    let mut outcome = Outcome::default();

    for recipient in &plan.recipients {
        let id = &recipient.person.slack_id;

        let invite = match minter.mint(ttl_secs) {
            Ok(i) => i,
            Err(e) => {
                let msg = format!("minting invite: {e}");
                ledger.record_invite(id, State::Failed, None, None, Some(&msg), now)?;
                outcome.failed.push((id.clone(), msg));
                continue;
            }
        };

        ledger.record_invite(
            id,
            State::Minted,
            Some((&invite.code, &invite.url, invite.expires_at)),
            None,
            None,
            now,
        )?;

        let expires = crate::fmt::datetime(invite.expires_at);
        let body = compose_dm(&recipient.person, community, &invite.url, &expires);

        match messenger.send_dm(id, &body) {
            Ok(dm) => {
                ledger.record_invite(
                    id,
                    State::Sent,
                    None,
                    Some((&dm.channel, &dm.ts)),
                    None,
                    now,
                )?;
                outcome.sent.push(id.clone());
            }
            Err(e) => {
                let msg = format!("sending DM: {e}");
                ledger.record_invite(id, State::Failed, None, None, Some(&msg), now)?;
                outcome.failed.push((id.clone(), msg));
            }
        }
    }

    // Excluded people are recorded too, so a later run does not reconsider
    // someone the operator already ruled out.
    for (person, reason) in &plan.excluded {
        if matches!(reason, Exclusion::AlreadyInvited) {
            continue;
        }
        ledger.record_invite(
            &person.slack_id,
            State::Skipped,
            None,
            None,
            Some(reason.describe()),
            now,
        )?;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::Export;
    use crate::parse::{self, Options};
    use crate::probe;
    use crate::selection::{self, Filter};
    use std::path::Path;

    /// Build an IR from the fixture the same way the CLI would.
    fn fixture_ir(channel_filter: &Filter) -> Vec<Record> {
        let mut export = Export::open(Path::new("fixtures/basic-export")).unwrap();
        let inventory = probe::probe(&mut export, false).unwrap();
        let resolved = selection::resolve(&inventory, channel_filter).unwrap();
        let mut out = Vec::new();
        parse::parse(
            &mut export,
            &inventory,
            &resolved.selected,
            &Options::default(),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn people_for(channel_filter: &Filter) -> Vec<Person> {
        candidates(&fixture_ir(channel_filter))
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.recipients
            .iter()
            .map(|r| r.person.slack_id.as_str())
            .collect()
    }

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn candidates_come_from_the_user_table() {
        let people = people_for(&Filter::public_only());
        let found: BTreeSet<&str> = people.iter().map(|p| p.slack_id.as_str()).collect();
        assert!(found.contains("U0ALICE"));
        assert!(
            found.contains("U0BUILDBOT"),
            "bots are candidates, then excluded"
        );
        assert!(found.contains("U0GONE"));
    }

    #[test]
    fn message_counts_reflect_only_the_imported_channels() {
        let public = people_for(&Filter::public_only());
        let alice_public = public
            .iter()
            .find(|p| p.slack_id == "U0ALICE")
            .unwrap()
            .message_count;

        let all = people_for(&Filter::everything());
        let alice_all = all
            .iter()
            .find(|p| p.slack_id == "U0ALICE")
            .unwrap()
            .message_count;

        assert!(
            alice_all > alice_public,
            "importing the DM adds Alice messages: {alice_public} → {alice_all}"
        );
    }

    #[test]
    fn candidates_are_ordered_most_active_first() {
        let people = people_for(&Filter::everything());
        let counts: Vec<usize> = people.iter().map(|p| p.message_count).collect();
        assert!(counts.windows(2).all(|w| w[0] >= w[1]), "{counts:?}");
    }

    #[test]
    fn bots_are_never_invited_even_when_named_explicitly() {
        let people = people_for(&Filter::public_only());
        let p = plan(
            &people,
            &PersonFilter {
                include: vec!["buildbot".to_string()],
                ..PersonFilter::default()
            },
            &none(),
        )
        .unwrap();
        assert!(p.is_empty(), "a named bot is still not invitable");
        assert!(p
            .excluded
            .iter()
            .any(|(person, r)| person.slack_id == "U0BUILDBOT" && *r == Exclusion::Bot));
    }

    #[test]
    fn deactivated_accounts_are_never_invited() {
        let people = people_for(&Filter::public_only());
        let p = plan(&people, &PersonFilter::default(), &none()).unwrap();
        assert!(!ids(&p).contains(&"U0GONE"));
        assert!(p
            .excluded
            .iter()
            .any(|(person, r)| person.slack_id == "U0GONE" && *r == Exclusion::Deactivated));
    }

    #[test]
    fn default_scope_is_members_of_imported_channels() {
        let people = people_for(&Filter::public_only());
        let p = plan(&people, &PersonFilter::default(), &none()).unwrap();
        // general's members are alice, pawel, bob, buildbot; buildbot is a bot.
        assert_eq!(ids(&p), vec!["U024BE7LH", "U0ALICE", "U0BOB"]);
    }

    /// The point of the default: importing only #general must not DM someone
    /// whose only membership is a channel that was left out.
    #[test]
    fn a_member_of_an_unimported_channel_is_not_invited() {
        // eng-private's members are pawel and bob. Import only the DM, whose
        // members are alice and bob — so pawel should not be a recipient.
        let people = people_for(&Filter {
            all_dms: true,
            ..Filter::default()
        });
        let p = plan(&people, &PersonFilter::default(), &none()).unwrap();
        assert_eq!(ids(&p), vec!["U0ALICE", "U0BOB"]);
        assert!(p
            .excluded
            .iter()
            .any(|(person, r)| person.slack_id == "U024BE7LH"
                && *r == Exclusion::NotInImportedChannels));
    }

    #[test]
    fn everyone_scope_ignores_channel_membership() {
        // U0GONE is in no channel, but is excluded as deactivated, not by scope.
        let people = people_for(&Filter {
            all_dms: true,
            ..Filter::default()
        });
        let p = plan(
            &people,
            &PersonFilter {
                scope: Scope::Everyone,
                ..PersonFilter::default()
            },
            &none(),
        )
        .unwrap();
        assert!(
            ids(&p).contains(&"U024BE7LH"),
            "not a DM member, still invited"
        );
    }

    #[test]
    fn posters_only_scope_drops_lurkers() {
        let people = people_for(&Filter::public_only());
        let p = plan(
            &people,
            &PersonFilter {
                scope: Scope::PostersOnly,
                ..PersonFilter::default()
            },
            &none(),
        )
        .unwrap();
        for r in &p.recipients {
            assert!(r.person.message_count > 0, "{:?}", r.person);
        }
    }

    #[test]
    fn explicit_users_can_be_named_by_handle_id_or_display_name() {
        let people = people_for(&Filter::public_only());
        for selector in ["alice", "ALICE", "@alice", "U0ALICE"] {
            let p = plan(
                &people,
                &PersonFilter {
                    include: vec![selector.to_string()],
                    ..PersonFilter::default()
                },
                &none(),
            )
            .unwrap();
            assert_eq!(ids(&p), vec!["U0ALICE"], "{selector}");
        }
    }

    #[test]
    fn a_typo_in_users_is_an_error_not_a_silent_skip() {
        let people = people_for(&Filter::public_only());
        let err = plan(
            &people,
            &PersonFilter {
                include: vec!["alise".to_string()],
                ..PersonFilter::default()
            },
            &none(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no one in this import matches"), "{err}");
    }

    #[test]
    fn a_typo_in_exclude_users_is_also_an_error() {
        let people = people_for(&Filter::public_only());
        assert!(plan(
            &people,
            &PersonFilter {
                exclude: vec!["nobody".to_string()],
                ..PersonFilter::default()
            },
            &none(),
        )
        .is_err());
    }

    #[test]
    fn exclude_users_removes_from_the_scope() {
        let people = people_for(&Filter::public_only());
        let p = plan(
            &people,
            &PersonFilter {
                exclude: vec!["bob".to_string()],
                ..PersonFilter::default()
            },
            &none(),
        )
        .unwrap();
        assert_eq!(ids(&p), vec!["U024BE7LH", "U0ALICE"]);
        assert!(p
            .excluded
            .iter()
            .any(|(person, r)| person.slack_id == "U0BOB" && *r == Exclusion::ExcludedByOperator));
    }

    /// The resume guarantee: nobody is DMed twice.
    #[test]
    fn already_invited_people_are_skipped_on_a_resumed_run() {
        let people = people_for(&Filter::public_only());
        let mut done = BTreeSet::new();
        done.insert("U0ALICE".to_string());

        let p = plan(&people, &PersonFilter::default(), &done).unwrap();
        assert!(!ids(&p).contains(&"U0ALICE"));
        assert!(p
            .excluded
            .iter()
            .any(|(person, r)| person.slack_id == "U0ALICE" && *r == Exclusion::AlreadyInvited));
    }

    #[test]
    fn every_candidate_is_either_a_recipient_or_an_explained_exclusion() {
        let people = people_for(&Filter::everything());
        let p = plan(&people, &PersonFilter::default(), &none()).unwrap();
        assert_eq!(
            p.recipients.len() + p.excluded.len(),
            people.len(),
            "nobody may be silently dropped"
        );
    }

    #[test]
    fn exclusion_counts_are_grouped_for_the_summary() {
        let people = people_for(&Filter::public_only());
        let p = plan(&people, &PersonFilter::default(), &none()).unwrap();
        let counts = p.exclusion_counts();
        assert_eq!(counts.get("is a bot"), Some(&1));
        assert_eq!(counts.get("account is deactivated"), Some(&1));
    }

    #[test]
    fn the_dm_says_what_it_is_and_that_the_archive_is_not_them() {
        let person = Person {
            slack_id: "U0ALICE".into(),
            name: "alice".into(),
            display_name: "Alice Anderson".into(),
            is_bot: false,
            is_deleted: false,
            channels: vec![],
            message_count: 3,
        };
        let dm = compose_dm(&person, "Acme", "https://acme.test/invite/x", "in 14 days");
        assert!(dm.contains("Alice Anderson"));
        assert!(dm.contains("https://acme.test/invite/x"));
        assert!(dm.contains("archive"), "must disclose what the history is");
        assert!(
            dm.contains("don't forward"),
            "invite codes are multi-use bearer tokens"
        );
    }

    // ── execute ──────────────────────────────────────────────────────────────

    fn fixture_plan() -> Plan {
        let people = people_for(&Filter::public_only());
        plan(&people, &PersonFilter::default(), &none()).unwrap()
    }

    #[test]
    fn a_dry_run_sends_nothing_but_walks_every_recipient() {
        let p = fixture_plan();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::DryRunMessenger::default();

        let outcome = execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();

        assert_eq!(outcome.sent.len(), p.len());
        assert!(outcome.is_complete());
        assert_eq!(
            messenger.sent.len(),
            p.len(),
            "the dry run exercises the same sequencing as the real path"
        );
    }

    #[test]
    fn an_under_privileged_key_fails_before_any_dm_is_sent() {
        let p = fixture_plan();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        minter.role = buzz::Role::Member;
        let mut messenger = slack::DryRunMessenger::default();

        let err = execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("only owners and admins"), "{err}");
        assert!(
            messenger.sent.is_empty(),
            "not a single DM may go out with an unusable key"
        );
    }

    #[test]
    fn one_recipients_failure_does_not_strand_the_rest() {
        let p = fixture_plan();
        let victim = p.recipients[0].person.slack_id.clone();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::FlakyMessenger {
            fail_for: vec![victim.clone()],
            sent: vec![],
        };

        let outcome = execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();

        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, victim);
        assert_eq!(outcome.sent.len(), p.len() - 1);
        assert!(!outcome.is_complete());
    }

    #[test]
    fn a_failed_dm_still_records_the_code_so_it_is_not_lost() {
        let p = fixture_plan();
        let victim = p.recipients[0].person.slack_id.clone();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::FlakyMessenger {
            fail_for: vec![victim.clone()],
            sent: vec![],
        };

        execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();

        let record = ledger.invite(&victim).unwrap().unwrap();
        assert_eq!(record.state, crate::ledger::State::Failed);
        assert!(
            record.invite_url.is_some(),
            "the minted code survives a failed send"
        );
    }

    /// The end-to-end resume guarantee, through the ledger rather than a set.
    #[test]
    fn a_second_run_does_not_dm_anyone_twice() {
        let people = people_for(&Filter::public_only());
        let ledger = crate::ledger::Ledger::in_memory().unwrap();

        let first = plan(&people, &PersonFilter::default(), &none()).unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::DryRunMessenger::default();
        let outcome = execute(
            &first,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();
        assert!(!outcome.sent.is_empty());

        // Re-plan from the ledger, as the CLI does on a resumed run.
        let done: BTreeSet<String> = ledger
            .invites_by_state(crate::ledger::State::Sent)
            .unwrap()
            .into_iter()
            .collect();
        let second = plan(&people, &PersonFilter::default(), &done).unwrap();

        assert!(
            second.is_empty(),
            "everyone already invited: {:?}",
            second.recipients
        );
        let mut messenger2 = slack::DryRunMessenger::default();
        let outcome2 = execute(
            &second,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger2,
            &ledger,
            200,
        )
        .unwrap();
        assert!(messenger2.sent.is_empty());
        assert!(outcome2.sent.is_empty());
    }

    /// A retry after a failure must actually re-send, not skip.
    #[test]
    fn a_failed_recipient_is_retried_on_the_next_run() {
        let people = people_for(&Filter::public_only());
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let first = plan(&people, &PersonFilter::default(), &none()).unwrap();
        let victim = first.recipients[0].person.slack_id.clone();

        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut flaky = slack::FlakyMessenger {
            fail_for: vec![victim.clone()],
            sent: vec![],
        };
        execute(
            &first,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut flaky,
            &ledger,
            100,
        )
        .unwrap();

        let done: BTreeSet<String> = ledger
            .invites_by_state(crate::ledger::State::Sent)
            .unwrap()
            .into_iter()
            .collect();
        let second = plan(&people, &PersonFilter::default(), &done).unwrap();

        let retried: Vec<&str> = second
            .recipients
            .iter()
            .map(|r| r.person.slack_id.as_str())
            .collect();
        assert_eq!(
            retried,
            vec![victim.as_str()],
            "only the failure is retried"
        );
    }

    #[test]
    fn excluded_people_are_recorded_so_they_are_not_reconsidered() {
        let p = fixture_plan();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::DryRunMessenger::default();
        execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();

        // The bot was excluded; it must be terminal in the ledger.
        assert!(ledger.invite_is_done("U0BUILDBOT").unwrap());
        assert_eq!(
            ledger.invite("U0BUILDBOT").unwrap().unwrap().state,
            crate::ledger::State::Skipped
        );
    }

    #[test]
    fn one_code_is_minted_per_recipient() {
        let p = fixture_plan();
        let ledger = crate::ledger::Ledger::in_memory().unwrap();
        let mut minter = buzz::DryRunMinter::new("acme.test");
        let mut messenger = slack::DryRunMessenger::default();
        execute(
            &p,
            "Acme",
            buzz::DEFAULT_TTL_SECS,
            &mut minter,
            &mut messenger,
            &ledger,
            100,
        )
        .unwrap();
        assert_eq!(minter.minted, p.len());

        // And each person's recorded URL is distinct.
        let urls: BTreeSet<String> = p
            .recipients
            .iter()
            .map(|r| {
                ledger
                    .invite(&r.person.slack_id)
                    .unwrap()
                    .unwrap()
                    .invite_url
                    .unwrap()
            })
            .collect();
        assert_eq!(urls.len(), p.len());
    }

    #[test]
    fn ir_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("s2b-ir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("import.jsonl");

        let mut export = Export::open(Path::new("fixtures/basic-export")).unwrap();
        let inventory = probe::probe(&mut export, false).unwrap();
        let resolved = selection::resolve(&inventory, &Filter::public_only()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        parse::parse(
            &mut export,
            &inventory,
            &resolved.selected,
            &Options::default(),
            &mut file,
        )
        .unwrap();
        drop(file);

        let loaded = load_ir(&path).unwrap();
        assert_eq!(loaded, fixture_ir(&Filter::public_only()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
