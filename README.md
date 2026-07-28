# slack2buzz

Imports Slack workspace history into a [Buzz](https://github.com/block/buzz)
community.

## Read this part first

**This is an archive importer, not an identity migration.** It cannot import
your colleagues' history *as* your colleagues, and it does not try to.

Buzz is a Nostr relay. Every message is an event signed by a keypair, and there
is no email address or username to match people on — only public keys, which
only their holders can sign for. Nothing this tool can do will produce messages
genuinely authored by the people who wrote them in Slack.

So it does the honest thing instead. Each Slack user gets a deterministic
**archive key**, derived from a master seed the operator supplies. Their
imported messages are signed with that key, and their display name is suffixed
so nobody is misled:

```
Paweł Zieliński [archive]
```

These keys are **custodial by construction** — whoever holds the seed can sign
as any of them. That is why:

- display names are always suffixed, with no flag to turn it off;
- the seed is destroyed after the import as the default path;
- the tool is loud about both.

They are not sovereign identities and are not meant to be adopted. If you want
your real identity on your own history, that needs a signature from your own
key — see `claim` in [DESIGN.md](DESIGN.md#m6-claim), which is deliberately not
built yet.

### Other things it does not do

- **It does not create accounts** for anyone. Nobody is signed up by an import.
- **It does not bring the files with it.** Slack exports contain *links* to
  files, not the files. Recovering them needs a separate token, and many will
  already be gone. See [docs/limitations.md](docs/limitations.md).
- **It does not fix timestamps for you.** See the blocker below.
- **It does not import what Slack did not export.** A free-plan export has no
  private channels and no DMs, whatever you asked Slack for. `slack2buzz probe`
  tells you which you actually got.

## Status

| Stage | What it does | State |
|---|---|---|
| `probe` | Report what an export contains | **works** |
| `parse` | Export → `import.jsonl` | **works** |
| `emit` | `import.jsonl` → signed Buzz events | **blocked**, see below |
| `invite` | DM Slack members a Buzz invite link | **plans and dry-runs**; `--execute` not wired |
| `claim` | Let people attest to their own archived history | not started |

### `emit` is blocked upstream

Importing history means publishing events with `created_at` in the past. Buzz
rejects that at two layers, and the second is not a policy knob:

1. Ingest rejects `|created_at - now| > 900s`.
2. A deferred constraint trigger (migration `0021_created_at_fence_floor.sql`)
   aborts, **at commit**, any channel-bearing event more than 960s old. The
   relay's writer pool arms it on every connection.

Layer 2 exists so the relay can prove a replica-freshness fence for cursor
pagination. Relaxing it for imports would make replica-served pages silently
*incomplete* — missing messages, not stale ones. So there is no client-side
capability that can do this, and this tool will not pretend otherwise by
backdating in a tag and calling it done.

The upstream requirement is filed as an RFC; backfill looks like an
operator-plane operation in Buzz's architecture. Until that resolves, `parse`
is fully usable and its output stays valid — the IR records true Slack
timestamps regardless of how `emit` eventually writes them.

## Install

The toolchain is pinned with [Hermit](https://cashapp.github.io/hermit/); no
global Rust install is needed.

```bash
git clone https://github.com/pawelz/slack2buzz && cd slack2buzz
. ./bin/activate-hermit
just build
```

## Use

Both commands below are read-only and send nothing anywhere.

Look at what you have:

```bash
slack2buzz probe export.zip
```

```
Export contains: public channels, private channels and direct messages
5 users, 4 conversations, 14 messages, 3 custom emoji

public (2):
  general                         10 msgs     2 thr     4 rxn    2 files  2024-03-04 → 2024-03-05
  tumbleweed                       0 msgs     0 thr     0 rxn    0 files  empty  [archived]
...
```

Convert it:

```bash
slack2buzz parse export.zip --all-public -o import.jsonl
```

`import.jsonl` is plain JSON Lines, one record per line, meant to be read and
hand-edited before anything is published.

### Choosing channels

Run `parse` with no selection flag on a terminal and it asks — a preset
(everything / all public / nothing), then a checkbox list showing each channel's
size and date range.

Non-interactively, be explicit:

```bash
# by kind
slack2buzz parse export.zip --all-public
slack2buzz parse export.zip --all              # includes private channels and DMs

# by name or id, comma-separated or repeated
slack2buzz parse export.zip --channels general,eng-private --channels C0GENERAL

# from a file, one per line, '#' comments allowed
slack2buzz parse export.zip --channels-file channels.txt

# subtract from a broader selection
slack2buzz parse export.zip --all --exclude random,watercooler
```

Two behaviours worth knowing:

- **A selector that matches nothing is an error.** `--channels genral` fails
  instead of quietly importing nothing under that name.
- **There is no implicit "everything".** With no TTY and no selection flag,
  `parse` refuses rather than guessing.

Archived and empty channels are excluded unless you name them explicitly or
pass `--include-archived` / `--include-empty`.

## Inviting people

`invite` works out who to DM from an `import.jsonl` and sends each of them a
Buzz invite link. **It sends nothing without `--execute`**, and `--execute` is
not wired up yet — the live Slack and Buzz clients are the remaining work. The
planning, selection, dry-run and resume ledger are done and tested.

```bash
slack2buzz invite import.jsonl --community "Acme Eng" --relay acme.buzz.example
```

A dry run prints the exact recipient list, why everyone else was left out, and
the verbatim DM body:

```
Will invite 3 of 5 people:
  Paweł Zieliński          @pawel
  alice                    @alice
  bob                      @bob

Not inviting:
     1  account is deactivated
     1  is a bot
```

### Who gets invited

By default, **members of the channels you actually imported** — nobody gets a DM
because of a channel you chose to leave out. Bots and deactivated accounts are
never invited, even if named explicitly.

On a terminal it asks: a preset (imported-channel members / posters only /
everyone / nothing preselected), then a checkbox list showing each person's
message count and channel count. Non-interactively:

```bash
slack2buzz invite import.jsonl ... --everyone          # whole export
slack2buzz invite import.jsonl ... --posters-only      # only people who posted
slack2buzz invite import.jsonl ... --users alice,bob   # named people only
slack2buzz invite import.jsonl ... --users-file pilot.txt
slack2buzz invite import.jsonl ... --exclude-users contractor
slack2buzz invite import.jsonl ... --list              # candidates, then exit
```

Same anti-footgun rules as channel selection: an unknown handle is an **error**,
and every exclusion is reported rather than silently applied. Unlike `parse`,
there *is* a defensible default here, so a non-interactive run proceeds with
imported-channel members rather than refusing.

### Things to know before you `--execute`

- **Invite codes are multi-use bearer tokens.** Buzz mints them as stateless
  HMAC tokens with no recipient binding, no use counter, and no per-code
  revocation (Buzz's own docs: revocation is "coarse — rotate the relay keypair").
  Anyone with the link can join. The DM says not to forward it, because that is
  the only control that exists.
- **Your Buzz key must be `owner` or `admin`.** A `member` key gets 403 from
  `POST /api/invites`. This is checked before the first DM, not after the fortieth.
- **The default TTL here is 14 days, not Buzz's 3.** A bulk invite goes to people
  who are on holiday; 72 hours kills most of the links unused. Buzz caps it at 30
  days and `--ttl-days` is clamped with a warning.
- **Mobile joins are broken upstream** — the link opens the community and hangs on
  "Connecting…". Invite a small pilot group first; the per-person picker exists
  partly for this.
- **The ledger is how nobody gets DMed twice.** A crash mid-run is resumed by
  re-running the same command. Delete `ledger.sqlite` only if you want everyone
  re-invited.

Required Slack token scopes: `chat:write` (plus `im:write` on some app configs).
Notably **not** `users:read.email` — no email address is used anywhere.

## How it works

```
Slack export .zip → [parse] → import.jsonl → [emit] → POST /events
```

Two stages, always. The intermediate representation is the point: it isolates
Slack's quirks from Buzz's event model, lets you inspect and edit before
publishing, lets emission be re-run without re-parsing, and lets someone write a
Discord or Teams parser against the same file. Same split as `mmetl` → `mmctl`.

`parse` is pure — no network, no clock — which is why its correctness is
testable, and why the golden files in `tests/golden/` are the real spec:

```bash
just test
just update-golden && git diff tests/golden   # to review an intentional change
```

See [DESIGN.md](DESIGN.md) for the identity, threading and ledger design, and
[docs/limitations.md](docs/limitations.md) for what this loses and why.

## Contributing

`just ci` before opening a PR. Same conventions as Buzz: `thiserror` in
libraries, `anyhow` in binaries, no `unwrap` outside tests, structured
`tracing`, Conventional Commits.

## Licence

Apache 2.0. See [LICENSE](LICENSE).
