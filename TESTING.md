# Testing

## What runs today

```bash
just check       # fmt-check, clippy -D warnings, tests, golden freshness
just test        # tests only
just update-golden && git diff tests/golden   # review an intentional IR change
```

140 tests. Three layers:

| Layer | Where | What it pins |
|---|---|---|
| Unit | `src/**` `mod tests` | mrkdwn normalisation, selection logic, ledger state machine, TTL clamping, date maths |
| Golden | `tests/golden.rs` + `tests/golden/*.jsonl` | the entire IR, byte for byte |
| Cross-reader | `tests/zip.rs` | `.zip` reads produce IR identical to directory reads |

The golden files are the real specification of `parse`. The unit tests assert
properties someone thought of; the golden files catch the changes nobody thought
to assert.

## What is *not* tested, in priority order

Being explicit, because "140 tests passing" invites more confidence than is
earned. The fixture is synthetic and written by the same person who wrote the
parser, so it only covers cases that were imagined.

### 1. Any real Slack export — the big one

Everything above runs against `fixtures/basic-export/`, which is hand-written.
A real export will contain things the fixture does not. Known suspects:

- **`thread_broadcast`** — a thread reply *also* posted to the channel. Currently
  imported as a plain reply, losing the broadcast flag. Buzz's `build_message`
  takes a `broadcast` argument and emits `["broadcast", "1"]`, so there is a real
  mapping being dropped. **This is a known fidelity bug, not just an untested
  path.**
- **`blocks`-only messages.** Slack apps post rich layout in `blocks` with `text`
  empty or a bare fallback. These currently import as empty or near-empty text.
- **Legacy `attachments`.** Older bot messages put their content there, not in
  `text`. Also dropped.
- **Subtypes with no handling**: `me_message`, `file_comment`, `channel_name`,
  `channel_archive`, `pinned_item`, `bot_add`, `reminder_add`, `huddle_thread`.
  Most are harmless as plain messages; some are noise that should be filtered.
- **Slack Connect / external members** — `<@U…>` ids absent from `users.json`,
  which fall back to the raw id.
- **`mpims.json`** (group DMs). `ChannelKind::GroupDm` exists and is unexercised.
- **`canvases/`, `lists/`, `huddle_transcripts/`** — filtered as metadata dirs,
  never actually seen by a test.
- **Unicode edge cases** in display names: RTL, zero-width joiners, emoji.
- **Duplicate `ts`** across day files. Exports have been observed to repeat.

### 2. Memory on a large export

`parse` buffers every IR record in memory before writing, because the header
carries the counts and must be written first. Fine for tens of thousands of
messages; unmeasured above that. Needs a generated large export and a
peak-RSS number before anyone points this at a decade of history.

### 3. The interactive pickers

Both `selection::prompt` and `invite::prompt` have zero coverage — `dialoguer`
wants a TTY. The *pure* halves they feed (`resolve`, `plan`) are thoroughly
tested, which is why the split exists, but the wiring between them is not.

### 4. The CLI itself

No test invokes the binary. Exit codes, flag parsing, and the
`--execute`-refuses-before-touching-the-ledger behaviour were verified by hand,
not in CI. That last one was a real bug found by hand — good evidence the gap
matters.

### 5. Everything network

`invite --execute` is unimplemented. `emit` does not exist. Neither `Messenger`
nor `Minter` has a live implementation, so nothing has ever talked to Slack or a
relay.

---

## The plan

Four tiers, cheapest and most informative first. Tiers 1–2 need nothing but a
laptop; 3–4 need real accounts and are where the risk lives.

### Tier 1 — extend the synthetic corpus (no external resources)

Highest value per hour, and it closes the known bug in §1.

1. **Fix `thread_broadcast`** and add it to the fixture. Carry a flag through the
   IR so `emit` can set Buzz's `broadcast` tag. This is a fidelity fix, not just
   a test.
2. **Add a second fixture, `fixtures/messy-export/`**, deliberately hostile:
   `blocks`-only messages, legacy `attachments`, every unhandled subtype, an
   external member missing from `users.json`, `mpims.json`, a `canvases/`
   directory, duplicate `ts`, RTL and emoji display names, an empty
   `channels.json`. Golden-file it. The fixture doubles as documentation of what
   we do with each oddity.
3. **Report unknown subtypes.** Have `probe` and `parse` count subtypes they have
   no specific handling for and print them. This converts every future unknown
   from a silent guess into a visible number — the single highest-leverage change
   for surviving real exports.
4. **CLI integration tests** (`assert_cmd`): every exit code, `-o -`, the
   non-interactive refusals, and `--execute` leaving no ledger behind.
5. **A generated large export** (~500k messages) behind `#[ignore]`, with a
   documented peak-RSS figure.

### Tier 2 — a real Slack export, still no network

The first contact with reality. Needs an export but no Buzz and no tokens.

- **Get an export you own.** Create a free Slack workspace, add a few channels,
  threads, reactions, file uploads, an app integration, then
  `Settings → Import/Export Data → Export`. Free tier exports **public channels
  only**, which is enough for most of §1.
- **Do not test against the company workspace casually.** A real export puts
  colleagues' messages on your disk, and this tool's whole point is that
  archives are sensitive. If private channels or DMs must be tested, do it on a
  throwaway paid workspace with synthetic conversations, not real history.
- **What to check**, in order:
  1. `probe` does not crash and its tier detection matches what Slack actually
     gave you.
  2. `skipped_unparseable` is **0**. Anything else is fidelity loss with a
     specific cause worth chasing.
  3. The unknown-subtype report from Tier 1.3 is empty, or every entry is
     understood.
  4. Spot-read 50 messages side by side against the Slack UI — mentions, links,
     code blocks, emphasis. This is the only way to catch normalisation that is
     *plausible but wrong*, which no automated check will find.
  5. Re-run `parse` twice and diff: output must be byte-identical.
- **Then fold what you learned back into `fixtures/messy-export/`**, redacted.
  A real bug that only reproduces on an export nobody else can see is a bug that
  comes back.

### Tier 3 — a local Buzz relay

Tests the Buzz half without involving Slack, and settles one open question
empirically.

- **Stand it up** from the pinned clone: `just dev` in `block/buzz` brings up
  Postgres, Redis and the relay via Docker. Build the CLI from source — not the
  one bundled in the desktop app, which has a known rustls panic on publish paths.
- **Confirm the `created_at` blocker for real.** Hand-sign a kind:9 event with a
  `created_at` a year in the past and `POST /events`. Two things worth knowing:
  1. the ingest rejection fires as read (±900s), and
  2. with the ingest check bypassed, migration 0021's trigger aborts the
     transaction **at commit**.

  This turns the analysis in [block/buzz#3306](https://github.com/block/buzz/issues/3306)
  from a code reading into a reproduction, which is worth a great deal in that
  discussion. **Do this before writing any more of `emit`.**
- **Test invite minting against it** once the live `Minter` exists: mint a real
  code, confirm an `owner`/`admin` key succeeds and a `member` key gets 403, and
  confirm the TTL clamp behaves as documented.
- **Then actually redeem a code** in a client, so we learn whether the mobile
  join bug affects the desktop path too.

### Tier 4 — a real Slack workspace, for `invite --execute`

The only tier that messages real people, so it goes last and stays small.

- **Use the throwaway workspace from Tier 2**, with two or three alt accounts you
  control. Never rehearse a bulk invite on colleagues.
- Create a Slack app, scope it `chat:write` (plus `im:write` if the config needs
  it), install it, take the bot token from the environment — never a flag, so it
  stays out of shell history.
- **What to verify:**
  1. Dry run first, always. The recipient list and DM body are exactly what you
     expect.
  2. `--execute` to **one** person. Read the DM as they receive it.
  3. Kill the process mid-run with several recipients queued, then re-run:
     **nobody is DMed twice**, and the person who failed *is* retried.
  4. Rate limiting: Slack's `chat.postMessage` tier permits roughly one message
     per second. Confirm the backoff is real rather than optimistic.
  5. Redeem one invite end to end, on desktop and on mobile, to characterise the
     upstream mobile bug.

### Tier 5 — full end-to-end

Only reachable once the upstream backfill question resolves. Import a real export
into a real relay, then verify in a client that threads attach, reactions land on
the right messages, and dates render correctly. Also the point at which the
audit-chain ordering caveat in [docs/limitations.md](docs/limitations.md) becomes
observable rather than theoretical.

---

## Suggested order

1. Tier 1.1 and 1.3 — fix `thread_broadcast`, add unknown-subtype reporting. Both
   are small and both change what Tier 2 can tell you.
2. Tier 2 — get a real export. Expect it to find things.
3. Tier 1.2 — encode what Tier 2 found as `messy-export` goldens.
4. Tier 3's blocker reproduction, to strengthen #3306.
5. Tier 1.4, then the live `Minter`/`Messenger`, then Tier 4.

Tier 2 is the one that will actually change our beliefs. Everything before it is
preparation for reading its results properly.
