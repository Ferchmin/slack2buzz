//! The Buzz side of `invite`: minting a join code.
//!
//! `POST /api/invites`, NIP-98 signed. Verified against `block/buzz` at
//! `137185e` (`crates/buzz-relay/src/api/invites.rs`,
//! `crates/buzz-relay/src/invite_token.rs`):
//!
//! - **The caller must be `owner` or `admin`** of the community. Authz mirrors
//!   kind:9030. A `member` key gets 403, so this must be checked *before* the
//!   first DM goes out rather than discovered halfway through.
//! - Request body is `{"ttl_secs": <u64>}` (optional; the whole body may be
//!   empty). Response is `{"code", "expires_at", "url"}`.
//! - TTL defaults to **72 hours** and is clamped to `[60s, 30 days]`.
//!
//! # Codes are multi-use bearer tokens
//!
//! This shapes the whole design, so it is worth stating plainly. An invite code
//! is a **stateless HMAC token**, not a database row: the payload is
//! `{c: community, r: "member", e: expires, n: nonce}` plus a MAC derived from
//! the relay keypair. Consequences:
//!
//! - It is **not bound to a recipient**. Anyone holding it can join.
//! - It is **multi-use** within its TTL. There is no use counter to decrement.
//! - It **cannot be revoked individually**. Buzz's own module docs say
//!   revocation is "coarse: rotate the relay keypair, or remove the member
//!   after the fact", and that per-code revocation awaits a future
//!   `relay_invites` table.
//!
//! So minting one code per person buys no enforcement — only an independent
//! nonce and the ability to correlate, in our own ledger, which link went to
//! whom. We do it anyway because it is cheap and that correlation is the only
//! forensic handle available, but the DM must tell people not to forward the
//! link, because forwarding it genuinely does admit strangers.
//!
//! The 72-hour default is also wrong for a bulk invite: DM 200 people on a
//! Friday and a large fraction of the links die unused. `invite` therefore asks
//! for a longer TTL explicitly rather than taking the default.

use crate::error::Result;

/// Default TTL this tool requests: 14 days.
///
/// Deliberately not Buzz's 72-hour default. A bulk invite goes to people who are
/// on holiday, heads-down, or simply not checking Slack, and a link that expires
/// before they act generates support load rather than members. Still well inside
/// the 30-day cap.
pub const DEFAULT_TTL_SECS: u64 = 14 * 24 * 60 * 60;

/// Buzz's hard cap (`invite_token::MAX_INVITE_TTL_SECS`).
pub const MAX_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Buzz's own default, for reference in help text.
pub const BUZZ_DEFAULT_TTL_SECS: u64 = 72 * 60 * 60;

// Enforced at build time rather than in a test: anyone editing the constants
// above should not be able to produce a TTL Buzz would silently clamp.
const _: () = assert!(DEFAULT_TTL_SECS > BUZZ_DEFAULT_TTL_SECS);
const _: () = assert!(DEFAULT_TTL_SECS <= MAX_TTL_SECS);

/// A minted invite, as returned by `POST /api/invites`.
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub code: String,
    pub url: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// The role the authenticated key holds in the community.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Member,
    None,
}

impl Role {
    /// Whether this role may mint invites. Mirrors the relay's check.
    pub fn can_mint(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// Mints Buzz invite codes.
pub trait Minter {
    /// The role our key holds. Checked once, before any DM is sent, so an
    /// under-privileged key fails with exit code 3 instead of after 40 DMs.
    fn role(&mut self) -> Result<Role>;

    /// Mint one invite code.
    fn mint(&mut self, ttl_secs: u64) -> Result<Invite>;
}

/// Mints nothing; produces obviously-fake codes. Used by `--dry-run`.
///
/// The URLs are deliberately unusable rather than plausible: a dry-run
/// transcript must not contain something a reader could mistake for a real
/// invite and paste into Slack themselves.
#[derive(Debug)]
pub struct DryRunMinter {
    pub host: String,
    pub role: Role,
    pub minted: usize,
    /// Unix seconds that expiries are measured from.
    ///
    /// Defaults to 0 so tests are reproducible without a clock. The CLI sets it
    /// to the real current time so a dry-run transcript shows the expiry date
    /// the operator would actually get, rather than a date in 1970.
    pub base_time: i64,
}

impl DryRunMinter {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            role: Role::Owner,
            minted: 0,
            base_time: 0,
        }
    }

    /// Measure expiries from `now` instead of the epoch.
    pub fn at(mut self, now: i64) -> Self {
        self.base_time = now;
        self
    }
}

impl Minter for DryRunMinter {
    fn role(&mut self) -> Result<Role> {
        Ok(self.role)
    }

    fn mint(&mut self, ttl_secs: u64) -> Result<Invite> {
        self.minted += 1;
        Ok(Invite {
            code: format!("DRY-RUN-NOT-A-REAL-CODE-{}", self.minted),
            url: format!(
                "https://{}/invite/DRY-RUN-NOT-A-REAL-CODE-{}",
                self.host, self.minted
            ),
            expires_at: self.base_time + ttl_secs as i64,
        })
    }
}

/// Clamp a requested TTL to what Buzz will actually honour, reporting when it
/// had to change so the operator is never silently given a different expiry
/// than they asked for.
pub fn clamp_ttl(requested: u64) -> (u64, Option<String>) {
    if requested > MAX_TTL_SECS {
        (
            MAX_TTL_SECS,
            Some(format!(
                "requested TTL of {requested}s exceeds Buzz's 30-day cap; using {MAX_TTL_SECS}s"
            )),
        )
    } else if requested < 60 {
        (
            60,
            Some(format!(
                "requested TTL of {requested}s is below Buzz's 60s floor; using 60s"
            )),
        )
    } else {
        (requested, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_owner_and_admin_can_mint() {
        assert!(Role::Owner.can_mint());
        assert!(Role::Admin.can_mint());
        assert!(!Role::Member.can_mint());
        assert!(!Role::None.can_mint());
    }

    #[test]
    fn ttl_over_the_cap_is_clamped_and_reported() {
        let (ttl, warning) = clamp_ttl(MAX_TTL_SECS + 1);
        assert_eq!(ttl, MAX_TTL_SECS);
        assert!(warning.unwrap().contains("30-day cap"));
    }

    #[test]
    fn ttl_under_the_floor_is_clamped_and_reported() {
        let (ttl, warning) = clamp_ttl(1);
        assert_eq!(ttl, 60);
        assert!(warning.unwrap().contains("60s floor"));
    }

    #[test]
    fn an_acceptable_ttl_passes_through_silently() {
        let (ttl, warning) = clamp_ttl(DEFAULT_TTL_SECS);
        assert_eq!(ttl, DEFAULT_TTL_SECS);
        assert_eq!(warning, None);
    }

    /// A dry-run transcript must not contain anything mistakable for a real
    /// invite — someone reading the output should not be able to paste a link.
    #[test]
    fn dry_run_codes_are_obviously_fake() {
        let mut m = DryRunMinter::new("acme.test");
        let invite = m.mint(3600).unwrap();
        assert!(invite.code.contains("DRY-RUN-NOT-A-REAL-CODE"));
        assert!(invite.url.contains("DRY-RUN-NOT-A-REAL-CODE"));
    }

    #[test]
    fn dry_run_codes_are_unique_per_mint() {
        let mut m = DryRunMinter::new("acme.test");
        let a = m.mint(60).unwrap();
        let b = m.mint(60).unwrap();
        assert_ne!(a.code, b.code, "one code per person, as in the real path");
    }
}
