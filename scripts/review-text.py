#!/usr/bin/env python3
"""Show messages whose text `parse` rewrote, so a human can check the rewrites.

The golden tests prove normalisation is *stable*. They cannot prove it is
*right* — only someone comparing against Slack can do that. This prints the
before and after for every message where the two differ, which is the smallest
useful unit of that review.

    scripts/review-text.py import.jsonl              # all rewritten messages
    scripts/review-text.py import.jsonl --limit 20
    scripts/review-text.py import.jsonl --subtype    # only messages with a subtype
    scripts/review-text.py import.jsonl --grep '```' # only ones containing a string

Open the same channel in Slack alongside and check that the "after" says the
same thing a human would read in the app. Things worth being suspicious of:
mentions, links with labels, bold/italic, code blocks, and anything with angle
brackets or ampersands in it.
"""

import argparse
import json
import sys


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ir", help="import.jsonl produced by `slack2buzz parse`")
    ap.add_argument("--limit", type=int, default=0, help="stop after N messages")
    ap.add_argument(
        "--subtype",
        action="store_true",
        help="only messages that carry a Slack subtype",
    )
    ap.add_argument("--grep", help="only messages whose raw text contains this")
    ap.add_argument(
        "--all",
        action="store_true",
        help="include messages whose text was not changed",
    )
    args = ap.parse_args()

    shown = 0
    total = 0
    unchanged = 0

    with open(args.ir, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            if record.get("type") != "message":
                continue
            total += 1

            raw, text = record.get("raw_text", ""), record.get("text", "")
            if raw == text:
                unchanged += 1
                if not args.all:
                    continue
            if args.subtype and not record.get("subtype"):
                continue
            if args.grep and args.grep not in raw:
                continue

            if args.limit and shown >= args.limit:
                break
            shown += 1

            flags = []
            if record.get("subtype"):
                flags.append(record["subtype"])
            if record.get("broadcast"):
                flags.append("broadcast")
            if record.get("thread_ts"):
                flags.append("in-thread")
            suffix = f"  [{', '.join(flags)}]" if flags else ""

            print(f"--- {record['slack_ts']}  {record.get('channel_slack_id','')}{suffix}")
            print(f"  slack : {raw}")
            print(f"  ours  : {text}")
            print()

    print(
        f"{total} messages, {total - unchanged} rewritten, {unchanged} unchanged; "
        f"showed {shown}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
