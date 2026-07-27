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
| `invite` | Invite Slack members to the community | not started |
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
