//! The resume ledger.
//!
//! Imports fail halfway. Always. A network blip at message 40,000 of 60,000, a
//! 429 storm, a laptop lid closing — and the only acceptable recovery is
//! re-running the same command and having it pick up where it stopped. That
//! requires durable per-record state, which is what this is.
//!
//! SQLite rather than a JSON file because the interesting operations are
//! "upsert one row" and "look up one key" against tens of thousands of rows,
//! repeatedly, with a crash possible between any two of them.
//!
//! Two tables, both keyed by the *Slack* identifier rather than anything Buzz
//! assigns. That is deliberate: the Slack id is the only thing that is stable
//! before the Buzz event exists, and therefore the only thing a resumed run can
//! look up.
//!
//! - `events` — `(channel_slack_id, slack_ts) → buzz_event_id`. Doubles as the
//!   thread map: `emit`'s second sub-pass resolves a reply's `thread_ts` by
//!   looking up the root's row. **Losing this table means thread replies can
//!   never be attached to their roots**, so it is durable state, not a cache.
//! - `invites` — `slack_user_id → invite code, DM outcome`. Stops a resumed run
//!   from DMing the same person twice.

use crate::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension};

/// How far a record got. Recorded rather than inferred so a resumed run can
/// distinguish "not attempted" from "attempted and failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Planned but not yet attempted.
    Planned,
    /// The Buzz side succeeded (event accepted, or invite minted).
    Minted,
    /// Fully done, including any outbound message.
    Sent,
    /// Attempted and failed. Carries an error string.
    Failed,
    /// Deliberately not attempted (filtered out, or already a member).
    Skipped,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Minted => "minted",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "planned" => Self::Planned,
            "minted" => Self::Minted,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }

    /// Whether a resumed run should leave this record alone.
    ///
    /// `Failed` is deliberately *not* terminal — retrying a failure is the
    /// entire point of resuming. `Skipped` is terminal because it reflects an
    /// operator decision, not an error.
    pub fn is_done(self) -> bool {
        matches!(self, Self::Sent | Self::Skipped)
    }
}

/// One invite's recorded outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct InviteRecord {
    pub slack_user_id: String,
    pub state: State,
    pub invite_url: Option<String>,
    pub expires_at: Option<i64>,
    pub error: Option<String>,
}

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Open (creating if needed) a ledger at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| Error::Ledger {
            context: format!("opening ledger {}", path.display()),
            source: e,
        })?;
        Self::init(conn)
    }

    /// An in-memory ledger, for tests and `--dry-run`.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(|e| Error::Ledger {
            context: "opening an in-memory ledger".into(),
            source: e,
        })?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL so a reader (a progress query) never blocks the writer, and so a
        // hard kill leaves a recoverable file rather than a truncated one.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                channel_slack_id TEXT NOT NULL,
                slack_ts         TEXT NOT NULL,
                buzz_event_id    TEXT,
                state            TEXT NOT NULL,
                error            TEXT,
                updated_at       INTEGER NOT NULL,
                PRIMARY KEY (channel_slack_id, slack_ts)
            );

            CREATE TABLE IF NOT EXISTS invites (
                slack_user_id TEXT PRIMARY KEY,
                invite_code   TEXT,
                invite_url    TEXT,
                expires_at    INTEGER,
                dm_channel    TEXT,
                dm_ts         TEXT,
                state         TEXT NOT NULL,
                error         TEXT,
                updated_at    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .map_err(|e| Error::Ledger {
            context: "creating ledger schema".into(),
            source: e,
        })?;

        Ok(Self { conn })
    }

    // ── invites ──────────────────────────────────────────────────────────────

    /// Record an invite outcome, replacing any previous row for that person.
    pub fn record_invite(
        &self,
        slack_user_id: &str,
        state: State,
        invite: Option<(&str, &str, i64)>,
        dm: Option<(&str, &str)>,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let (code, url, expires) = match invite {
            Some((c, u, e)) => (Some(c), Some(u), Some(e)),
            None => (None, None, None),
        };
        let (dm_channel, dm_ts) = match dm {
            Some((c, t)) => (Some(c), Some(t)),
            None => (None, None),
        };

        self.conn
            .execute(
                "INSERT INTO invites
                   (slack_user_id, invite_code, invite_url, expires_at,
                    dm_channel, dm_ts, state, error, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(slack_user_id) DO UPDATE SET
                   invite_code = COALESCE(excluded.invite_code, invites.invite_code),
                   invite_url  = COALESCE(excluded.invite_url,  invites.invite_url),
                   expires_at  = COALESCE(excluded.expires_at,  invites.expires_at),
                   dm_channel  = COALESCE(excluded.dm_channel,  invites.dm_channel),
                   dm_ts       = COALESCE(excluded.dm_ts,       invites.dm_ts),
                   state       = excluded.state,
                   error       = excluded.error,
                   updated_at  = excluded.updated_at",
                rusqlite::params![
                    slack_user_id,
                    code,
                    url,
                    expires,
                    dm_channel,
                    dm_ts,
                    state.as_str(),
                    error,
                    now,
                ],
            )
            .map_err(|e| Error::Ledger {
                context: format!("recording invite for {slack_user_id}"),
                source: e,
            })?;
        Ok(())
    }

    pub fn invite(&self, slack_user_id: &str) -> Result<Option<InviteRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT state, invite_url, expires_at, error
                   FROM invites WHERE slack_user_id = ?1",
                [slack_user_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| Error::Ledger {
                context: format!("reading invite for {slack_user_id}"),
                source: e,
            })?;

        Ok(
            row.map(|(state, invite_url, expires_at, error)| InviteRecord {
                slack_user_id: slack_user_id.to_string(),
                state: State::parse(&state).unwrap_or(State::Failed),
                invite_url,
                expires_at,
                error,
            }),
        )
    }

    /// Whether a resumed run should skip this person.
    pub fn invite_is_done(&self, slack_user_id: &str) -> Result<bool> {
        Ok(self
            .invite(slack_user_id)?
            .is_some_and(|r| r.state.is_done()))
    }

    pub fn invites_by_state(&self, state: State) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT slack_user_id FROM invites WHERE state = ?1 ORDER BY slack_user_id")?;
        let ids = stmt
            .query_map([state.as_str()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    // ── events (used by `emit`; the thread map lives here) ───────────────────

    pub fn record_event(
        &self,
        channel_slack_id: &str,
        slack_ts: &str,
        buzz_event_id: Option<&str>,
        state: State,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO events
                   (channel_slack_id, slack_ts, buzz_event_id, state, error, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(channel_slack_id, slack_ts) DO UPDATE SET
                   buzz_event_id = COALESCE(excluded.buzz_event_id, events.buzz_event_id),
                   state         = excluded.state,
                   error         = excluded.error,
                   updated_at    = excluded.updated_at",
                rusqlite::params![
                    channel_slack_id,
                    slack_ts,
                    buzz_event_id,
                    state.as_str(),
                    error,
                    now
                ],
            )
            .map_err(|e| Error::Ledger {
                context: format!("recording event {channel_slack_id}/{slack_ts}"),
                source: e,
            })?;
        Ok(())
    }

    /// Resolve a Slack `ts` to the Buzz event id it produced.
    ///
    /// This is the thread map. `emit`'s reply pass calls it for every reply's
    /// `thread_ts`; a `None` here means the root was never successfully
    /// published and the reply must be deferred rather than orphaned.
    pub fn buzz_event_id(&self, channel_slack_id: &str, slack_ts: &str) -> Result<Option<String>> {
        let id = self
            .conn
            .query_row(
                "SELECT buzz_event_id FROM events
                  WHERE channel_slack_id = ?1 AND slack_ts = ?2",
                [channel_slack_id, slack_ts],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| Error::Ledger {
                context: "reading thread map".into(),
                source: e,
            })?;
        Ok(id.flatten())
    }

    pub fn event_is_done(&self, channel_slack_id: &str, slack_ts: &str) -> Result<bool> {
        let state = self
            .conn
            .query_row(
                "SELECT state FROM events WHERE channel_slack_id = ?1 AND slack_ts = ?2",
                [channel_slack_id, slack_ts],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| Error::Ledger {
                context: "reading event state".into(),
                source: e,
            })?;
        Ok(state
            .and_then(|s| State::parse(&s))
            .is_some_and(State::is_done))
    }

    // ── meta ─────────────────────────────────────────────────────────────────

    /// Store a scalar. Used to bind a ledger to one import so a stale ledger
    /// cannot be silently reused against a different export or community.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> Ledger {
        Ledger::in_memory().unwrap()
    }

    #[test]
    fn an_unknown_invite_is_absent_not_an_error() {
        let l = ledger();
        assert_eq!(l.invite("U0NOBODY").unwrap(), None);
        assert!(!l.invite_is_done("U0NOBODY").unwrap());
    }

    #[test]
    fn a_sent_invite_is_done_and_a_resumed_run_skips_it() {
        let l = ledger();
        l.record_invite(
            "U0ALICE",
            State::Sent,
            Some(("code1", "https://x.test/invite/code1", 999)),
            Some(("D0DM", "1.2")),
            None,
            100,
        )
        .unwrap();
        assert!(l.invite_is_done("U0ALICE").unwrap());
        let r = l.invite("U0ALICE").unwrap().unwrap();
        assert_eq!(r.state, State::Sent);
        assert_eq!(r.invite_url.as_deref(), Some("https://x.test/invite/code1"));
    }

    /// The point of a resume ledger: a failure must be retried, not skipped.
    #[test]
    fn a_failed_invite_is_not_done_so_it_will_be_retried() {
        let l = ledger();
        l.record_invite("U0BOB", State::Failed, None, None, Some("429"), 100)
            .unwrap();
        assert!(!l.invite_is_done("U0BOB").unwrap());
        assert_eq!(
            l.invite("U0BOB").unwrap().unwrap().error.as_deref(),
            Some("429")
        );
    }

    /// A skipped person reflects an operator decision, so it is terminal.
    #[test]
    fn a_skipped_invite_is_done() {
        let l = ledger();
        l.record_invite("U0BOT", State::Skipped, None, None, None, 100)
            .unwrap();
        assert!(l.invite_is_done("U0BOT").unwrap());
    }

    /// Minting then sending must not lose the code: the second write only
    /// changes state, and COALESCE keeps the earlier columns.
    #[test]
    fn a_later_write_does_not_erase_an_earlier_invite_code() {
        let l = ledger();
        l.record_invite(
            "U0ALICE",
            State::Minted,
            Some(("code1", "https://x.test/i/code1", 999)),
            None,
            None,
            100,
        )
        .unwrap();
        // The send step knows the DM ids but not the code.
        l.record_invite(
            "U0ALICE",
            State::Sent,
            None,
            Some(("D0DM", "1.2")),
            None,
            200,
        )
        .unwrap();

        let r = l.invite("U0ALICE").unwrap().unwrap();
        assert_eq!(r.state, State::Sent);
        assert_eq!(
            r.invite_url.as_deref(),
            Some("https://x.test/i/code1"),
            "the code survived the state transition"
        );
        assert_eq!(r.expires_at, Some(999));
    }

    /// Retrying a failure must clear the stale error, or the final report
    /// claims a success also failed.
    #[test]
    fn a_successful_retry_clears_the_previous_error() {
        let l = ledger();
        l.record_invite("U0BOB", State::Failed, None, None, Some("429"), 100)
            .unwrap();
        l.record_invite(
            "U0BOB",
            State::Sent,
            Some(("c", "u", 1)),
            Some(("D", "1.0")),
            None,
            200,
        )
        .unwrap();
        let r = l.invite("U0BOB").unwrap().unwrap();
        assert_eq!(r.state, State::Sent);
        assert_eq!(r.error, None);
    }

    #[test]
    fn invites_can_be_listed_by_state() {
        let l = ledger();
        l.record_invite("U0B", State::Sent, None, None, None, 1)
            .unwrap();
        l.record_invite("U0A", State::Sent, None, None, None, 1)
            .unwrap();
        l.record_invite("U0C", State::Failed, None, None, Some("x"), 1)
            .unwrap();
        assert_eq!(l.invites_by_state(State::Sent).unwrap(), vec!["U0A", "U0B"]);
        assert_eq!(l.invites_by_state(State::Failed).unwrap(), vec!["U0C"]);
    }

    // ── the thread map ───────────────────────────────────────────────────────

    #[test]
    fn the_thread_map_resolves_a_slack_ts_to_a_buzz_event_id() {
        let l = ledger();
        l.record_event("C0G", "100.1", Some("abc123"), State::Sent, None, 1)
            .unwrap();
        assert_eq!(
            l.buzz_event_id("C0G", "100.1").unwrap(),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn an_unpublished_root_resolves_to_none_so_replies_can_be_deferred() {
        let l = ledger();
        // Attempted, failed, no event id — a reply must not be orphaned onto it.
        l.record_event("C0G", "100.1", None, State::Failed, Some("boom"), 1)
            .unwrap();
        assert_eq!(l.buzz_event_id("C0G", "100.1").unwrap(), None);
        assert!(!l.event_is_done("C0G", "100.1").unwrap());
    }

    #[test]
    fn events_are_keyed_per_channel_so_identical_ts_do_not_collide() {
        let l = ledger();
        l.record_event("C0A", "100.1", Some("aaa"), State::Sent, None, 1)
            .unwrap();
        l.record_event("C0B", "100.1", Some("bbb"), State::Sent, None, 1)
            .unwrap();
        assert_eq!(l.buzz_event_id("C0A", "100.1").unwrap().unwrap(), "aaa");
        assert_eq!(l.buzz_event_id("C0B", "100.1").unwrap().unwrap(), "bbb");
    }

    #[test]
    fn re_recording_an_event_is_idempotent() {
        let l = ledger();
        for _ in 0..3 {
            l.record_event("C0G", "100.1", Some("abc"), State::Sent, None, 1)
                .unwrap();
        }
        assert!(l.event_is_done("C0G", "100.1").unwrap());
        assert_eq!(l.buzz_event_id("C0G", "100.1").unwrap().unwrap(), "abc");
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let l = ledger();
        assert_eq!(l.meta("community").unwrap(), None);
        l.set_meta("community", "abc").unwrap();
        l.set_meta("community", "def").unwrap();
        assert_eq!(l.meta("community").unwrap(), Some("def".to_string()));
    }

    #[test]
    fn a_ledger_survives_being_closed_and_reopened() {
        let dir = std::env::temp_dir().join(format!("s2b-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.sqlite");
        let _ = std::fs::remove_file(&path);

        {
            let l = Ledger::open(&path).unwrap();
            l.record_event("C0G", "100.1", Some("abc"), State::Sent, None, 1)
                .unwrap();
        }
        {
            let l = Ledger::open(&path).unwrap();
            assert_eq!(
                l.buzz_event_id("C0G", "100.1").unwrap(),
                Some("abc".to_string()),
                "the thread map must outlive the process"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
