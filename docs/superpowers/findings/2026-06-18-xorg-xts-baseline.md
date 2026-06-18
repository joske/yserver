# Xorg XTS baseline — the real target profile (2026-06-18)

**Why this exists:** we long assumed real Xorg passes XTS 100%, so any XTS FAIL on yserver was "our bug." That assumption is **false**. This is a full XTS run against **bare-metal Xorg on a TTY** (eiger, aarch64) — the reference implementation — captured so the actual target is "match Xorg's per-case verdict profile", not an absolute pass count.

## Artifacts

- `2026-06-18-xorg-xts-baseline.tsv` — per-`(case, test-purpose)` verdicts, extracted via `tools/xts-vs-baseline.py extract`. This is the diff target.
- `2026-06-18-xorg-xts-journal.gz` — the raw TET journal (gzipped, source of truth; regenerate the TSV with `python3 tools/xts-vs-baseline.py extract <(zcat …)`).

## Headline: Xorg is NOT 100%

5987 test purposes:

| Verdict | Count |
|---|---|
| PASS | 4635 |
| UNTESTED | 715 |
| NOTINUSE | 273 |
| **FAIL** | **209** |
| UNSUPPORTED | 97 |
| UNRESOLVED | 39 |
| WARNING | 17 |
| NORESULT | 2 |

**209 FAIL on stock Xorg.** Per-suite FAIL (top): Xlib9 **68**, XI **50**, Xlib13 **28**, Xlib3 10, XIproto 8, Xlib14 7, Xt13/Xlib7 5, … These are unwinnable on yserver too — the reference fails them (64-bit `KeySym` marshalling, deprecated behaviours, env/keymap assumptions). Chasing them was the 2026-06-18 detour.

## How to use it

```
# yserver run -> journal; then:
python3 tools/xts-vs-baseline.py diff \
  docs/superpowers/findings/2026-06-18-xorg-xts-baseline.tsv \
  <yserver-journal>
```

The **REGRESSIONS** bucket (Xorg PASS, yserver not) is the real bug list. The "candidate stricter" bucket (yserver PASS, Xorg FAIL) is informational. Everything Xorg also fails drops out. See memory `reference-xorg-not-100pct-on-xts-xi`.
