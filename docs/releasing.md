# Cutting a release

Checklist for `.github/workflows/release.yml`. Everything here is enforced by
the workflow — this doc exists so the enforcement is not the first place you
find out.

Two repositories are involved: this one, and
[`joske/yserver-packaging`](https://github.com/joske/yserver-packaging), which
builds the Fedora/Debian/Alpine packages the release ships. **Both need a
version bump, and the packaging one is easy to forget** — its build refuses to
run when the recipes disagree with `Cargo.toml`.

## 1. Bump the version

In this repo:

- `Cargo.toml:13` — the **only** version reference in the tree (verify with
  `grep -rn '<old version>' --include='*.toml' --include='*.rs' .`).
- `Cargo.lock` — run `cargo check` and commit the result; the five
  workspace crates (`yserver`, `yserver-core`, `yserver-protocol`,
  `yserver-render`, `yserver-xauth`) follow.

In `yserver-packaging`, all three recipes, which `build.yml` requires to equal
`Cargo.toml` exactly:

| File | Field |
|---|---|
| `alpine/APKBUILD` | `pkgver=X.Y.Z`, `pkgrel=0` |
| `fedora/yserver.spec` | `Version: X.Y.Z`, `Release: 1%{?dist}`, plus a new `%changelog` entry `- X.Y.Z-1` |
| `debian/changelog` | a new top entry `yserver (X.Y.Z-1) unstable; urgency=medium` with an RFC-2822 signature trailer |

Do **not** hand-compute `sha512sums` in the APKBUILD. The `update-checksum`
job in `build.yml` recomputes it after a successful `v*` build and pushes it to
packaging `master` itself, so `git pull` there once the release is out. A stale
committed value is a notice, never a build failure — the build jobs let
`abuild checksum` recompute.

Push both repos. Neither push publishes anything: `build.yml` is
`workflow_dispatch` only.

## 2. Tag

An **annotated** tag, `vX.Y.Z`, message `yserver X.Y.Z`:

```sh
git tag -a v1.5.0 -m 'yserver 1.5.0'
git push origin v1.5.0
```

The grammar `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$` is enforced, so
`v1.5` and `v1.5.0 bad` are refused. Pushing the tag does not release
anything; publishing is a dispatch, not a side effect of tagging.

**Prereleases are not supported without a decision.** `v1.5.0-rc.1` passes the
tag grammar but is refused twice — once in `checks`, once in `aur` — because an
Arch `pkgver` cannot contain `-`. Pick a mapping (`1.5.0rc.1`) and teach the
workflow before tagging one.

## 3. Dispatch

First look, because a dispatch is one-shot and public:

```sh
gh run list --workflow=release.yml -L 5
```

The concurrency group is `release` with `cancel-in-progress: false`, so a
second dispatch **queues behind** the first rather than replacing it — two
runs for the same tag is a mess, not a no-op.

Then dispatch **with the tag as the ref**:

```sh
gh workflow run release.yml --ref v1.5.0
```

`--ref master` is rejected by design: `checks` requires `github.ref_type` to
be `tag`, rather than inferring one from HEAD (`git tag --points-at HEAD`
would happily pick a tag that merely happens to sit on a branch tip, and
would silently choose between several).

## 4. What the stages do

`checks` → `ci` → `packages` → `release` → `aur`. Nothing is public until
`release`.

**`checks`** — cheap and side-effect-free. Ref is a tag; tag matches the
grammar; tag resolves to the run's commit; `Cargo.toml` matches the tag; the
tag exists on `origin`; no *published* release exists for it; the version is a
legal Arch `pkgver`. Then it fully preflights the AUR credential — key parses
unencrypted, host fingerprint matches the pinned value, clone succeeds,
`receive-pack` reachable, and `list-repos` says this account owns `yserver`.
That preflight is here rather than in `aur` because `aur` runs *after*
publication. Both of the workflow's secrets are checked before anything is
public, and each failure message says what to fix.

**`ci`** — `ci.yml` as a reusable workflow, from the tag's tree.

**`packages`** — dispatches `build.yml` in the packaging repo with
`-f ref=<tag> -f sha=<commit>` and waits (240 × 15 s ≈ 60 min cap). The run id
comes from the dispatch response; correlating by run title was removed
deliberately, since a title is not unique and another run's packages could be
attached as ours. **This is where a forgotten recipe bump fails**, in about
8 s:

```
recipe version(s) disagree with upstream 1.5.0 — bump the recipes
```

Fix the recipes, push packaging, re-dispatch. Nothing was published.

**`release`** — creates the release as a **draft**, downloads the packaging
artifacts, attaches the `.rpm`/`.deb`/`.apk` and the Windows zips from `ci` (debuginfo, debugsource, dbgsym
and `.src.rpm` excluded: ~40 MB of ~46 MB), asserts the attached inventory
matches what was downloaded exactly, reports the tag archive's sha256/sha512
in the job summary, re-verifies the tag has not moved, and only then
`gh release edit --draft=false`. The draft is an intermediate state inside
this job, not the end state — a green run leaves a **published** release.

A leftover draft from a failed run is replaced automatically, and uploads use
`--clobber`, so **re-dispatching the same tag is safe**. A *published* release
blocks the run; delete it first to re-release.

**`aur`** — bumps `pkgver`/`pkgrel`/`sha256sums` in the AUR PKGBUILD,
regenerates `.SRCINFO` in a throwaway directory as an unprivileged user, and
pushes. It runs after publication, so its contract is: the GitHub release is
authoritative; if this fails, the release stays up and this job is re-run once
the cause is fixed. Cross-system publication is not atomic and the workflow
does not pretend otherwise.

## 5. After

- **Fill in the notes.** `--generate-notes` lists **merged PRs only**, so work
  pushed straight to master does not appear. Recover it with
  `git log --reverse --format='%h|%s' <prev tag>..<tag>`, drop the PR squashes
  and the `docs`/`chore`/`ci` commits, and append the rest to `## What's
  Changed` as `* <subject> in <commit URL>`. `gh release edit --notes` is a
  full replace — read the current body first. Better still, merge user-visible
  work as a PR so this step is empty.
- `git pull` in `yserver-packaging` for the bot's `alpine: sha512sums` commit.
- The job summary has the source-archive hashes if a downstream recipe needs
  them by hand.
- Close the milestone / issues the release resolves.
