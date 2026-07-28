# Design

Why slack2buzz is shaped the way it is. [README.md](README.md) covers what it
does; [docs/limitations.md](docs/limitations.md) covers what it loses. This
covers the decisions.

Pinned to `block/buzz` at `137185e`.

## Two stages, non-negotiable

```
Slack export .zip → [parse] → import.jsonl (IR) → [emit] → POST /events
```

A single-stage importer would be shorter and worse. The IR exists because:

- **Slack's quirks and Buzz's event model change independently.** Buzz is 0.4.x
  with event kinds still moving. Coupling a Slack parser directly to a moving
  event schema means every upstream kind change touches parsing code.
- **Imports must be inspectable before they are irreversible.** Publishing to an
  append-only, hash-chained relay is not undoable in any practical sense. A
  human needs to be able to read and edit what is about to be published.
- **Emission must be re-runnable without re-parsing.** Imports fail halfway.
  Re-reading a multi-gigabyte zip to retry the last 3% is wasteful and, worse,
  invites the parse step to produce something subtly different the second time.
- **Someone else should be able to write a Discord parser.** The IR is the
  extension point. Nothing in `src/ir.rs` mentions kinds, pubkeys, or signatures.

This mirrors `mmetl` → `mmctl` for Mattermost, which got the split right.

`parse` is pure: no network, no clock, no randomness. That is what makes golden
files meaningful, and `tests/golden/` is the real specification of parse
behaviour.

## Identity: deterministic archive keys

`key_i = HMAC-SHA256(master_seed, slack_user_id)`

The alternatives and why they lose:

| Approach | Problem |
|---|---|
| One key for the whole import | Every message has the same author. Search by person, per-author threading, and any sense of who said what are all gone. |
| Random key per user | Re-running the import creates a second set of authors for the same people. Not idempotent, so a resumed import duplicates identities. |
| Ask each person for a key | Requires everyone to be onboarded *before* any history exists — exactly backwards, and most will never respond. |
| Deterministic derived keys | Readable names, working author search, stable across re-runs. Custodial. |

Custodial is a real cost, not a footnote: whoever holds the seed can sign as any
archived person, forever. The mitigations are structural rather than advisory:

- The `[archive]` display-name suffix has no override flag.
- Destroying the seed is the default path after import, not an option.
- Documentation never describes these as anyone's identity.

The seed is the operator's to protect and then destroy. There is no key escrow,
because a key escrow is exactly the thing that would make these look adoptable.

## Timestamps

`created_at` comes from the Slack `ts`. There is no fallback mode.

This is currently blocked upstream — see
[docs/limitations.md](docs/limitations.md#timestamps-an-unresolved-upstream-blocker)
for the two enforcement layers and why the second is load-bearing rather than a
policy knob.

The decision to ship no fallback is deliberate. An archive where every message
claims to have been written on import day is not a cheaper version of a correct
archive; it is a different and misleading artifact. The IR records true Slack
timestamps regardless, so `parse` output stays valid whatever `emit` eventually
does.

## Event mapping

Confirmed against `crates/buzz-core/src/kind.rs` and
`crates/buzz-sdk/src/builders.rs`:

| IR record | Buzz kind | Notes |
|---|---|---|
| `message` | **9** (`KIND_STREAM_MESSAGE`) | `["h", <channel_uuid>]`. Not kind 1 — kind 1 is `KIND_TEXT_NOTE` and is not the channel message. |
| `message` (edited) | 40003 | `KIND_STREAM_MESSAGE_EDIT` |
| `reaction` | **7** (`KIND_REACTION`) | content is the emoji, `["e", <target>]` |
| `reaction` (custom emoji) | 7 | content `:shortcode:` plus `["emoji", shortcode, url]` |
| `channel` | 9007 | NIP-29 create; `h` optional, channel does not exist yet |
| `channel` topic/purpose | 9002 | metadata edit |
| `user` | 0 | profile |
| `file` | 1063 | NIP-94 file metadata |
| `emoji` | 30030 | per-member emoji set; the palette is a read-side union |

`emit` must use `buzz-sdk`'s typed builders and `buzz-core` for signing. Do not
reimplement canonicalisation: Buzz's canonical JSON uses `BTreeMap` for
deterministic key ordering and the Schnorr signature depends on it byte for
byte.

## Provenance

Every emitted event carries:

```json
["imported_from", "slack", "<team_id>/<channel_id>/<slack_ts>"]
```

This works against an unmodified relay today — `events.tags` is `JSONB` with no
allowlist — and it is already indexed, because `idx_events_tags_gin`
(migration 0004, `jsonb_path_ops`) makes `tags @> '[["imported_from","slack"]]'`
an index probe rather than a scan. That gives cheap idempotency checks and gives
clients a cheap "this is archived content" signal.

The spelling is proposed upstream so that this importer, the Slack Connect
bridge (block/buzz#2822), and any future Discord or Teams importer agree on one
convention rather than each inventing their own.

## Threads: two sub-passes

Slack identifies a thread by its root's `ts`. Buzz threads reference the parent's
**event id**, which does not exist until the parent is published. So:

1. Emit all root and un-threaded messages. Record `slack_ts → buzz_event_id` in
   the ledger as each is accepted.
2. Emit thread replies, resolving `thread_ts` against that map.

The map must be **persisted**, not held in memory. Losing it means replies can
never be attached to their roots, and re-deriving it requires querying the relay
for every message already published. The ledger is the durable copy.

`parse` marks roots and replies but does not reorder them — `is_thread_root()`
and `is_thread_reply()` on `ir::Message` are derived from `thread_ts`, and `emit`
does the two-pass ordering.

## Ledger

SQLite. Imports fail halfway — always — so resumability is a design requirement
rather than a nicety.

Keyed by `(channel_slack_id, slack_ts)` for messages and
`(channel_slack_id, target_slack_ts, emoji, reactor)` for reactions, which is why
`slack_ts` is preserved verbatim in the IR and never reformatted. Records the
resulting `buzz_event_id`, so:

- a re-run skips what was already accepted (idempotent);
- the thread map survives a crash;
- a partial import can be reported per-record rather than as a single failure.

## Channel selection

Selection is split into a pure resolver (`selection::Filter` → `Resolved`) and a
thin interactive picker. Everything that decides what gets imported lives in the
pure half, where it is unit tested.

Two rules exist specifically to prevent quiet mistakes:

- **An unknown selector is an error.** Silently skipping a misspelled channel
  name is indistinguishable from success until the archive is published.
- **Non-interactive runs never default to everything.** No TTY and no selection
  flag means refusal. Private channels and DMs are in scope for exactly the
  operators least likely to want them swept in by accident.

The interactive flow is two steps — a preset, then a checkbox list seeded from it
— because that makes select-all and deselect-all single keystrokes while still
allowing per-channel toggles.

## Invites

The brief's sketch was `conversations.members` → `users.list` → invite → DM.
Reading the export and Buzz's invite code changed two things about that.

**Planning needs no Slack API.** `chat.postMessage` accepts a user id directly as
its `channel`, opening the DM implicitly, and the export already contains channel
membership and the user table. So there is no `conversations.open`, no
`users.list`, and no email address anywhere — planning is pure and testable, and
a token is needed only to send. The cost is staleness: someone who left after the
export was taken still looks active. The export's own `deleted` flag covers
anyone deactivated *before* the export, which is the common case; a
`--verify-directory` pass would close the rest and is not built.

**Invite codes are multi-use bearer tokens**, which changes what per-person
minting means. Verified in `crates/buzz-relay/src/invite_token.rs`: a code is a
stateless HMAC token carrying `{c: community, r: "member", e: expires, n: nonce}`,
not a database row. It is not bound to a recipient, it is multi-use within its
TTL, and it cannot be revoked individually — Buzz's own module docs say
revocation is "coarse: rotate the relay keypair, or remove the member after the
fact", with per-code revocation awaiting a future `relay_invites` table.

So minting one code per person buys **no enforcement**, only an independent nonce
and the ability to correlate in our own ledger which link went to whom. We still
do it, because it is cheap and that correlation is the only forensic handle
available — but the honest consequence is that the DM must tell people not to
forward the link, since forwarding genuinely does admit strangers.

Two other findings shaped the defaults:

- **Minting requires `owner` or `admin`** (authz mirrors kind:9030). A `member`
  key gets 403. Checked once, before the first DM, so an under-privileged key
  fails immediately rather than after forty messages.
- **Buzz's default TTL is 72 hours**, clamped to `[60s, 30 days]`. That is too
  short for a bulk invite — DM 200 people on a Friday and most links die unused.
  This tool asks for 14 days explicitly.

The per-recipient sequence is mint → record `Minted` → send → record `Sent`. The
record-before-send is deliberate: a crash in between leaves the code in the
ledger rather than losing it, and only `Sent` marks a person done, so a crash
before that retries them. `Failed` is deliberately *not* terminal — retrying is
the point of a ledger — while `Skipped` is, because it reflects an operator
decision rather than an error.

Both network boundaries are traits (`invite::slack::Messenger`,
`invite::buzz::Minter`) so the dry-run path and the real path are structurally
identical code with a different implementation swapped in. A dry run therefore
exercises the real sequencing and ledger writes, rather than being a separate
branch that happens to print. The dry-run minter emits deliberately unusable
codes (`DRY-RUN-NOT-A-REAL-CODE-…`) so nothing in a transcript can be mistaken
for a real invite and pasted into Slack.

## Milestones

- **M0 `probe`** — inventory an export; derive which Slack tier it represents.
  Ships standalone. *Done, minus relay metadata and auth check, which are held
  until the `emit` transport question resolves.*
- **M1 `parse`** — export → IR, with golden tests. *Done.*
- **M2 `emit`** — channels and root messages, resume ledger, `--skip-files`,
  `--skip-reactions`. *Blocked on the timestamp question.*
- **M3** — threads and reactions.
- **M4** — files, avatars, custom emoji, partial-failure reporting.
- **M5 `invite`** — *planning, selection, dry run and ledger done; live senders
  outstanding.* See below.
- **M6 `claim`** — a person joins with their own keypair and publishes one signed
  attestation ("Slack user U024BE7LH is me"); clients render archived messages
  with their real identity, sourced from their own signature. This mirrors Buzz's
  own agent model, where a second signature ties an agent to its human owner —
  see `docs/nips/NIP-OA.md` (Owner Attestation) and NIP-IA's explicit
  non-goal that owner requests "do not make the owner the author of the agent's
  historical events". Build only if M2–M5 see real use.
