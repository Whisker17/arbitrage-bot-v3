#!/usr/bin/env python3
"""Check that the saved Dune SQL equals the repo SQL body (offline, no Dune access).

body(file) = the file with its leading `--` comment header and the blank lines
after it removed, and without the trailing newline. That is the convention used
when publishing: the Dune editor holds the SQL without the repo header.

Usage: body_check.py <publication-dir>
The directory is the external publication record. It must contain
new-8781215.sql and new-8781229.sql (the published bodies),
readback-8781215-v12.json and readback-8781229-v5.json (post-update readbacks),
and readonly-8781227-v4.json and readonly-8781231-v7.json (read-only checks
of the unchanged dependents). Exits non-zero on any mismatch.
"""
import json
import pathlib
import sys

REPO = pathlib.Path(__file__).resolve().parents[3]
SQL = REPO / "scripts" / "dunesql"

# (repo file, query id, published body file or None, readback json)
CASES = [
    ("00_qualified_arbs.sql", "8781215", "new-8781215.sql", "readback-8781215-v12.json"),
    ("01_discover_bots.sql", "8781227", None, "readonly-8781227-v4.json"),
    ("02_arb_detail_feed.sql", "8781229", "new-8781229.sql", "readback-8781229-v5.json"),
    ("03_bot_strategy_profile.sql", "8781231", None, "readonly-8781231-v7.json"),
]


def body(text):
    lines = text.split("\n")
    i = 0
    while i < len(lines) and lines[i].startswith("--"):
        i += 1
    header = i
    while i < len(lines) and lines[i].strip() == "":
        i += 1
    return header, "\n".join(lines[i:]).rstrip("\n")


def main():
    pub = pathlib.Path(sys.argv[1])
    ok = True
    for fname, qid, new, readback in CASES:
        header, b = body((SQL / fname).read_text())
        saved = json.loads((pub / readback).read_text())
        checks = [("readback", saved["query"] == b), ("is_private", saved["is_private"] is True)]
        if new:
            checks.append(("published", (pub / new).read_text() == b))
        line = " ".join(f"{k}={'OK' if v else 'MISMATCH'}" for k, v in checks)
        print(f"{fname} (q{qid} v{saved['version']}): header={header} lines, body={len(b)} chars, {line}")
        ok &= all(v for _, v in checks)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
