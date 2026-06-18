#!/usr/bin/env python3
"""Diff XTS verdicts against a reference (Xorg) baseline.

Real Xorg on a TTY does NOT pass the XTS suites 100% (the XI suite alone
fails ~47/316 cases on stock Xorg — see memory). So the meaningful target
for yserver is *matching Xorg's per-case verdict profile*, not an absolute
pass count. This tool extracts per-(case, test-purpose) verdicts from a TET
journal and diffs a candidate run against a baseline run, surfacing only the
cases where the two disagree — which is the actual yserver bug list.

Usage:
  # Snapshot a reference run (e.g. Xorg on a TTY) into a baseline file:
  tools/xts-vs-baseline.py extract <journal> > baselines/xorg-XI.tsv

  # Diff a candidate run (yserver) against that baseline:
  tools/xts-vs-baseline.py diff baselines/xorg-XI.tsv <yserver-journal>

  # Or diff two journals directly:
  tools/xts-vs-baseline.py diff <baseline-journal> <candidate-journal>

`diff` exit code is 0 when there are no REGRESSIONs (baseline PASS that the
candidate does not pass), 1 otherwise — so it is usable as a CI gate.
"""
from __future__ import annotations

import sys


def parse_journal(path: str) -> dict[tuple[str, str], str]:
    """journal -> {(case_path, tp): verdict_string}.

    `10|<tc> /Scenario/Case <time>|TC Start...` binds a tc number to a case
    path; `220|<tc> <tp> <code> <time>|VERDICT` is the verdict for one test
    purpose. We key on (case_path, tp) so results are independent of the
    sequential tc index (which varies with scenario/run ordering).
    """
    tc_to_case: dict[str, str] = {}
    verdicts: dict[tuple[str, str], str] = {}
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            parts = line.rstrip("\n").split("|")
            if len(parts) < 3:
                continue
            code = parts[0]
            if code == "10":
                # parts[1] = "<tc> /Scenario/Case <time>"
                fields = parts[1].split()
                if len(fields) >= 2:
                    tc_to_case[fields[0]] = fields[1]
            elif code == "220":
                fields = parts[1].split()
                if len(fields) >= 2:
                    tc, tp = fields[0], fields[1]
                    case = tc_to_case.get(tc, f"tc{tc}")
                    verdicts[(case, tp)] = parts[2].strip()
    return verdicts


def load(path: str) -> dict[tuple[str, str], str]:
    """Load a baseline TSV (case<TAB>tp<TAB>verdict) or a raw journal."""
    with open(path, encoding="utf-8", errors="replace") as fh:
        head = fh.readline()
    if "\t" in head:
        out: dict[tuple[str, str], str] = {}
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                cols = line.rstrip("\n").split("\t")
                if len(cols) == 3:
                    out[(cols[0], cols[1])] = cols[2]
        return out
    return parse_journal(path)


def cmd_extract(journal: str) -> int:
    v = parse_journal(journal)
    for (case, tp), verdict in sorted(v.items()):
        print(f"{case}\t{tp}\t{verdict}")
    return 0


# Verdicts that count as "the implementation handled it correctly". Anything
# else from the candidate where the baseline PASSed is a regression.
_PASSING = {"PASS", "UNSUPPORTED", "NOTINUSE", "UNTESTED", "NORESULT"}


def cmd_diff(baseline_path: str, candidate_path: str) -> int:
    base = load(baseline_path)
    cand = load(candidate_path)
    common = sorted(base.keys() & cand.keys())
    only_base = sorted(base.keys() - cand.keys())
    only_cand = sorted(cand.keys() - base.keys())

    regressions = []  # baseline PASS, candidate not -> real yserver bug
    stricter = []     # baseline FAIL, candidate PASS -> we pass what Xorg fails
    other = []        # any other verdict mismatch
    for key in common:
        b, c = base[key], cand[key]
        if b == c:
            continue
        if b == "PASS" and c not in _PASSING:
            regressions.append((key, b, c))
        elif b != "PASS" and c == "PASS":
            stricter.append((key, b, c))
        else:
            other.append((key, b, c))

    def show(title, rows):
        print(f"\n== {title} ({len(rows)}) ==")
        for (case, tp), b, c in rows:
            print(f"  {case} tp{tp}: baseline={b} candidate={c}")

    print(f"baseline={baseline_path}  candidate={candidate_path}")
    print(f"common purposes: {len(common)}  agree: "
          f"{sum(1 for k in common if base[k] == cand[k])}")
    show("REGRESSIONS (baseline PASS, candidate not) — real bug list", regressions)
    show("candidate stricter (baseline !PASS, candidate PASS)", stricter)
    show("other verdict mismatches", other)
    if only_base:
        print(f"\n(only in baseline: {len(only_base)} purposes)")
    if only_cand:
        print(f"(only in candidate: {len(only_cand)} purposes)")
    return 1 if regressions else 0


def main(argv: list[str]) -> int:
    if len(argv) >= 3 and argv[1] == "extract":
        return cmd_extract(argv[2])
    if len(argv) >= 4 and argv[1] == "diff":
        return cmd_diff(argv[2], argv[3])
    print(__doc__)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
