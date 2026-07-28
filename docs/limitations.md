# Limitations

What this tool loses, gets wrong, or cannot do — and why. Read it before
importing anything you cannot re-import.

Verified against `block/buzz` at commit `137185e` (tag `v0.4.26`; mobile has
moved to `0.5.0-rc`). Buzz is pre-1.0 with a large open-issue count and event
kinds still in motion, so treat every relay-side claim here as pinned to that
commit and expect to chase.

## Identity: archived, not migrated

The headline limitation, restated because it is the one people skim past.

Imported messages are **not** authored by the people who wrote them. They are
signed by deterministic archive keys, `key_i = HMAC-SHA256(master_seed,
slack_user_id)`, and whoever holds the seed can sign as any of them. The keys
buy readable names in the UI, working author search, and stable idempotent
re-runs. They are custodial, and they are not identities.

Consequences to be clear-eyed about:

- Anyone with the seed can forge new messages that appear to come from any
  archived person. **Destroy the seed after the import** — that is the default
  path, not an option.
- Archive keys must never be handed to the people they represent as "your key".
  They are shared-custody by construction.
- The `[archive]` display-name suffix is not cosmetic. It is the only thing
  distinguishing archived history from live messages in most client views.

## Timestamps: an unresolved upstream blocker

Buzz will not accept historical `created_at` through any client path.

- Ingest rejects `|created_at - now| > 900s`
  (`crates/buzz-relay/src/handlers/ingest.rs`).
- A `DEFERRABLE INITIALLY DEFERRED` constraint trigger from
  `migrations/0021_created_at_fence_floor.sql` aborts, at COMMIT, any insert of
  a channel-bearing `events` row older than 960s. The relay's writer pool arms
  it on every connection via the `buzz.created_at_floor` GUC.

The trigger is load-bearing: it proves the replica-freshness fence that keyset
cursor pagination relies on (`crates/buzz-db/src/replica_fence.rs`). Reads route
to a replica only for rows older than the fence, so a backfilled row committing
below it would make replica-served pages **silently incomplete**.

Migration 0021's own comment prescribes the alternative — backfill runs on a
connection *without* the GUC, outside the relay's writer pool, with the replica
breaker held closed until the WAL replays. That makes historical backfill an
operator-plane operation requiring relay operator access, which in turn means
**it can never work against a hosted Buzz instance**, only a self-hosted one.

This tool deliberately ships no fallback. Publishing at `now()` with the real
timestamp hidden in a tag would produce an archive whose every message claims to
have been written on import day, and no client renders that correctly today.

## Audit chain disagrees with message dates

Buzz keeps a hash-chained audit log where each entry's SHA-256 covers
`prev_hash`, chained per community (`crates/buzz-audit/src/entry.rs`).

Backfilled events chain in **import order**, not chronological order. Anything
that reads that chain as a timeline will disagree with the message dates. This
is inherent to appending history to an append-only chain — there is no ordering
that satisfies both — so it is documented rather than papered over.

## Files: expect partial loss

Slack exports contain **links** to files, not the files.

- Recovering them needs a separate Slack token with file scope, beyond what the
  export itself required.
- `url_private` links expire and require authentication.
- Files may already be deleted. Free-plan workspaces tombstone them
  (`mode: "hidden_by_limit"`); `parse` records these as `is_deleted` so no doomed
  fetch is attempted.
- Externally hosted files (Google Drive and similar) have no bytes to migrate at
  all, only a URL worth preserving.

Budget more time for files than for messages, threads and reactions combined.
v1 ships `--skip-files` and expects it to be the common path.

## Relay-side constraints

- **`POST /events` is one event per request.** It takes a single signed event
  and returns one `{event_id, accepted, message}`
  (`crates/buzz-relay/src/api/bridge.rs`). There is no batch endpoint, and each
  request needs its own NIP-98 auth event. It beats `buzz-cli` for bulk work —
  one process per message does not scale — but throughput planning and 429
  backoff must assume per-event round trips.
- **`buzz-auth` rate limits.** Honour `429` with real backoff.
- **Do not use the `buzz-cli` bundled in the desktop app for scripting.** It has
  a known rustls `CryptoProvider` panic on some publish paths that needs an
  app-bundle fix. Build the CLI from source.
- **Mobile invite joins are broken upstream** — the invite opens the community
  and hangs on "Connecting…", which limits who can be onboarded at all. Relevant
  to `invite` (M5) more than to the archive itself.

## Invites

- **Codes are multi-use bearer tokens with no revocation.** An invite code is a
  stateless HMAC token (`crates/buzz-relay/src/invite_token.rs`) carrying
  `{community, role, expiry, nonce}`. It is not bound to a recipient, it is
  reusable within its TTL, and it cannot be revoked individually — Buzz's own
  docs describe revocation as "coarse: rotate the relay keypair, or remove the
  member after the fact". Minting one per person gives correlation in our ledger,
  not enforcement. Treat each DM as containing a shared secret.
- **Invites expire, and Buzz caps the TTL at 30 days.** This tool defaults to 14
  rather than Buzz's 3, but anyone who does not act in time needs a fresh invite.
- **Minting requires an `owner` or `admin` key.** A `member` key cannot invite.
- **The candidate list is as stale as the export.** Someone who left after the
  export was taken still appears active; only accounts already deactivated at
  export time are filtered. A `users.list` verification pass is not built.
- **Messages that are only Slack `blocks`** contribute no author signal, so a
  person whose entire history is app-rendered content may show 0 messages and be
  missed by `--posters-only`.
- **`--execute` is not implemented.** The live `chat.postMessage` and
  NIP-98-signed `POST /api/invites` clients are outstanding; the command refuses
  rather than silently dry-running.

## Parsing fidelity

What `parse` knowingly does not preserve:

- **Join/leave messages** are dropped by default as membership churn rather than
  conversation. `--keep-joins` retains them; the count of dropped messages is
  always reported.
- **Blocks and attachments** are not interpreted. Slack's `blocks` and
  `attachments` arrays carry rich layout; only the top-level `text` fallback is
  normalised. Messages that are *only* blocks (some app messages) will import as
  empty text. The raw JSON is not retained for these — only `raw_text` is.
- **Emphasis conversion is heuristic.** Slack's `*bold*` becomes `**bold**` when
  the delimiters hug non-whitespace and the span holds no newline. This
  deliberately declines to convert `2 * 3 * 4`, and it will therefore also
  decline some genuinely bold text spanning lines.
- **Mentions render as display names**, which may contain spaces
  (`@Paweł Zieliński`). Slack renders them the same way, but the text is no
  longer a parseable handle.
- **Custom emoji aliases** are recorded verbatim (`alias:other`) and not
  resolved.
- **Messages without a `ts`** are skipped and counted. `ts` is a message's
  identity and the ledger's idempotency key; a synthetic one would break
  re-runs.
- **Slack's `ts` sub-second component is a disambiguator, not precision.** It is
  preserved in `slack_ts` for identity and dropped from `created_at`.
- **Shared/Connect channels** are not specially handled. External members appear
  as ordinary user ids that may not be in `users.json`, in which case mentions
  fall back to the raw id.

## Things `probe` will tell you that Slack will not

- Whether the export actually contains private channels and DMs, or only public
  channels. This is derived from which manifests exist, never asked — the export
  is ground truth and operator belief frequently is not.
- Channels listed in a manifest with no messages in the export (usually channels
  the exporter had no access to).
- Message directories with no manifest entry, which will *not* be imported.
