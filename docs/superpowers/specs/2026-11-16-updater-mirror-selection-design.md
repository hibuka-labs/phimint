# Design: automatic mirror selection for the updater

Date: 2026-11-16
Status: proposed

## Problem

`phimint update` hangs or fails for users on mainland China.

Measured on a China-hosted machine (2026-11-16), against the 0.11.0 release:

| Path | Result |
|---|---|
| `github.com` TCP connect | Intermittent — several connect timeouts, then a 0.09s connect |
| `gitee.com` TCP connect | Stable — 0.07–0.22s connect, 0.4–0.6s total, 3/3 |
| GitHub asset, 25 MB | 41 KB/s → ~600 s to finish |
| Gitee asset, same 25 MB | 2.0 MB/s → 11.9 s |
| Gitee asset SHA-256 vs manifest | Identical (`ea57fbaa…82e2`) |

Two distinct defects cause this.

**Defect 1 — manifest endpoint order is hardcoded GitHub-first.**
`src/model/config.rs:387` (`default_endpoints`) returns GitHub then Gitee.
`src/update/checker.rs:42` walks that list sequentially with a 3s timeout per
endpoint (`src/update/checker.rs:67`). A China user pays a full connect timeout
on GitHub before Gitee is ever tried.

**Defect 2 — the download URL is baked into the manifest.**
`src/update/apply.rs:95` downloads `entry.url`, the URL carried by whichever
manifest was fetched. There is no fallback to the other mirror. Because the
manifest fetch is racy (GitHub sometimes answers in 0.3s), a China user can
land on the GitHub manifest, receive a GitHub asset URL, and then stall at
41 KB/s. The download timeout is 120s (`src/update/apply.rs:97`), and 120s at
41 KB/s transfers 5 MB of the 25 MB archive — so the download fails outright.

Note that no URL rewriting is needed to fix this. The release pipeline already
emits per-mirror manifests: `scripts/gen-manifest.py:95` writes
`manifest.json` (GitHub URLs) and `manifest-gitee.json` (Gitee URLs), and
`deploy/release.sh:306` uploads the Gitee variant renamed to `manifest.json`
on the Gitee release. The Gitee manifest's `platforms[].url` already points at
`gitee.com` — verified on the live `v0.11.0` release. Selecting the right
manifest endpoint therefore selects the right asset host automatically.

**Adjacent defect (separate scope, reported not fixed here).**
`src/model/config.rs:352` lists
`https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/tips.json`
as the Gitee tips endpoint. It returns 404: `tips.json` is not a release asset
(the v0.11.0 GitHub release asset list contains no `tips.json`) and
`deploy/release.sh` never publishes one to the Gitee metadata branch. The
README's claim that "the updater falls back to the Gitee mirror automatically"
is false for tips.

## Goals

1. `phimint update` succeeds for a user on mainland China without any manual
   configuration.
2. The mechanism works for a user outside China, including behind a proxy
   that reaches GitHub but not Gitee, and vice versa.
3. No user data or IP address leaves the machine for the purpose of deciding.
4. An explicit override remains available for users who want to pin a mirror.

## Non-goals

- Publishing `tips.json` to the Gitee mirror (reported as a separate defect).
- Changing the release pipeline.
- Rewriting asset URLs inside the manifest.

## Approach: concurrent race + asset fallback

Decide the mirror by measuring the mirrors themselves, not by inferring the
user's country. Two properties follow:

- It answers the question that actually matters: which mirror is fast *from
  here*. A user in Singapore on a China ISP line gets Gitee; a user in
  Shanghai with a working proxy gets GitHub. Country inference gets both
  wrong.
- It needs no third-party geo-IP service, so no user IP is disclosed and no
  external service can be slow, rate-limited, or blocked.

A third-party geo-IP service was considered and rejected: `ipinfo.io/country`
and `ip-api.com/json/` do answer `CN` from a China address, but they answer
the wrong question (where the user is, not which mirror is fast), they require
disclosing the user's IP, and they are themselves subject to the same
in-country reachability problem.

### Component 1: mirror endpoint catalogue

A new module `src/update/mirror.rs` owns the mirror topology. It exposes the
two built-in endpoints:

```
GitHub  https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json
Gitee  https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/manifest.json
```

Note the Gitee URL uses the `releases/download/latest/` form rather than the
existing `raw/release-metadata/` form. The `raw` form answers 302 and redirects
through `raw.giteeusercontent.com`; the release-asset form is a direct 200 and
is the same asset set. `deploy/release.sh:306` guarantees the file is present
at that path with Gitee asset URLs inside.

Each endpoint carries an identity so the asset fallback can find the same
version on the other mirror.

### Component 2: racing the manifest fetch

`src/update/checker.rs` gains a race path used when the caller asks for
automatic selection. All endpoints are fetched concurrently; the first
response that parses as a valid manifest wins. The others are dropped.

The per-endpoint timeout stays at 3s for the probe — but a connect timeout
is what actually hurts, so the client gains a `connect_timeout` of 2s
distinct from the 3s overall timeout. A China user pays ~2s once instead of
3s serially per unreachable endpoint.

A race has one subtlety: "first to respond" can favour a slow-but-alive
endpoint over a fast one if the fast one happens to lose a scheduling race.
For a 2-endpoint race where one endpoint is usually entirely unreachable
(connect timeout), first-response is the right metric. It is not the right
metric when both are reachable, but in that case either choice downloads the
same bytes from an equivalent mirror, so the outcome is equivalent.

**The race keeps the losing manifest.** This is what makes the asset
fallback free: the runner-up's manifest is already in memory, so the
fallback asset URL needs no extra network round-trip. The loser's manifest is
retained only when it parsed successfully; if the loser failed to connect
there is nothing to retain, and the fallback fetches it on demand (see
Component 3).

### Component 3: asset fallback on download failure

This is the part that fixes the reported hang.

When the winner's asset download fails — timeout, connection reset, or a
truncated body — the updater retries the same version from the other mirror.
The fallback URL comes from the other mirror's manifest: the platform entry
for the same version and the same platform key.

Two cases:

- The runner-up's manifest was already parsed during the race. Its platform
  entry gives the fallback URL with no extra network cost.
- The runner-up failed outright during the race. Its manifest is fetched now,
  and the platform entry for the winning version is looked up in it. If that
  manifest reports a *different* version than the winner, the fallback is
  skipped: the mirrors disagree about what is current, and installing either
  is a guess.

The retry must preserve the checksum contract. Both mirrors publish the same
archive with the same SHA-256 (`scripts/gen-manifest.py:72` derives both
manifests from one `sums` map, and the release uploads the same built
artifacts to both hosts). The fallback download therefore verifies against
the *winning* manifest's expected SHA-256, not the fallback manifest's — so a
mirror serving a divergent artifact is caught by the checksum rather than
silently installed.

The download timeout must also change: 120s is enough for a 2 MB/s link but
not for a 41 KB/s link. It becomes 300s, and the fallback triggers when the
timeout is hit or the rate floor trips. With Gitee at 2 MB/s the normal path
takes 12s; with GitHub degraded to 41 KB/s the primary attempt is not
abandoned (it is above the floor) and completes in ~600s, which exceeds even
the 300s timeout — so the timeout fires and the fallback completes in 12s,
for a 5-minute worst case. The rate floor covers the case where GitHub is
stalled rather than merely slow, cutting that to ~20s.

A faster primary signal is added alongside the timeout: the download is read in
chunks and the rate is measured over the first window. If the rate is below a
usable floor, the primary mirror is abandoned early rather than waiting out the
full timeout.

The floor is 32 KiB/s sustained over the first 8 seconds, and the check only
fires once at least 64 KiB has arrived — otherwise a slow start would be
mistaken for a slow link. The numbers are chosen so that:

- Gitee's measured 2.0 MB/s is 60× above the floor.
- GitHub's measured 41 KB/s is only 1.3× above it, so a merely-degraded GitHub
  is *not* abandoned: it still finishes, just slowly. Only a link that is
  genuinely not moving (a stalled or heavily-throttled connection) trips the
  floor.

The worst case therefore becomes roughly 8s-to-detect plus the fallback's
12s, rather than 300s. A link between 32 KiB/s and 41 KB/s still uses the
full timeout, which is the intended behaviour: it is slow, but it works.

A mirror that is merely slow is a lesser problem than a mirror that never
finishes. The rate floor exists to bound the worst case, not to optimise the
common case.

### Component 4: explicit override

Two override surfaces, both optional:

- `PHIMINT_MIRROR=gitee|github` — the same variable `install.sh:57` already
  uses. The Rust updater currently ignores it entirely (`grep PHIMINT_MIRROR
  src/` has no hits), so honouring it also makes the installer and the updater
  agree. When set, the race is skipped and that single endpoint is used.
- `update.endpoints` in the config file (`src/model/config.rs:366`) — already
  supported. When a user sets it explicitly, the config wins and the race is
  skipped; the user's explicit list is their contract.

Precedence: explicit config `endpoints` > `PHIMINT_MIRROR` > automatic race.

### Component 5: the tips mirror

The same race is applied to the tips fetch (`src/tips/mod.rs:113`), which walks
its endpoint list the same way the update checker does. But the Gitee tips
endpoint 404s today, so racing it gains nothing until `tips.json` is
published to the Gitee mirror.

Decision: the tips race is implemented, and the broken Gitee tips endpoint is
removed from `default_tips_endpoints()` as part of this change, with a
comment recording that it 404s and that re-adding it requires publishing
`tips.json` to Gitee first. Publishing that file is out of scope but is the
documented prerequisite for restoring Gitee tips.

## Data flow

```
phimint update
  └─ resolve endpoints
       ├─ config update.endpoints set?  → use it, single-source, no race
       ├─ PHIMINT_MIRROR set?           → use that one, no race
       └─ otherwise                     → race {GitHub, Gitee} concurrently
            └─ first valid manifest wins; runner-up's manifest kept in memory
                 ├─ no upgrade → "up to date"
                 └─ upgrade available
                      └─ download asset from winner's URL (300s timeout)
                           ├─ ok → verify SHA-256 → install
                           └─ fail → take the other mirror's platform entry
                                    for the SAME version (runner-up manifest
                                    if the race got it, else fetch it now)
                                    └─ download that asset
                                         └─ verify against the WINNER's
                                            SHA-256 → install
```

## Error handling

- Every network failure is non-fatal to the product: the update check is
  silent (already the case in `src/shell/wire.rs:75`).
- A checksum mismatch on either mirror aborts the update. A divergent artifact
  is never installed, whichever mirror served it.
- If all mirrors fail, `phimint update` prints the failure and exits non-zero
  with a message naming the endpoints tried, so the user can act.

## Testing

Unit tests (no network), in the existing `#[cfg(test)] mod tests` style:

- Endpoint catalogue returns both mirrors, and the Gitee URL is the
  release-asset form.
- The race returns the first valid manifest and ignores a malformed one.
- The race skips endpoints that fail to connect.
- The race retains the runner-up's manifest for the fallback.
- The asset fallback derives the other mirror's URL for the same version and
  the same platform key, from the retained runner-up manifest.
- The fallback fetches the other manifest on demand when the race did not
  retain one, and is skipped when that manifest reports a different version.
- The fallback verifies against the winning manifest's SHA-256.
- A checksum mismatch on the fallback aborts.
- `PHIMINT_MIRROR` forces a single endpoint and skips the race.
- Explicit config `endpoints` wins over `PHIMINT_MIRROR`.

Integration tests (network, opt-in like the existing suite):

- `phimint update --check` from a China-networked host resolves via Gitee and
  reports the version.

Script-level test, in `tests/install_script_test.sh` style:

- The installer's `auto` mode picks the reachable mirror. The existing
  installer already tries GitHub then Gitee sequentially; it gains the same
  concurrent-probe behaviour so a China user does not wait out a GitHub
  connect timeout before the index fetch.

## Files touched

- `src/update/mirror.rs` (new) — the mirror catalogue and URL derivation.
- `src/update/checker.rs` — the race, and returning which mirror won so the
  caller can fall back.
- `src/update/apply.rs` — the download timeout, the rate floor, and the
  fallback download.
- `src/main.rs` — wiring the endpoints and the override precedence for
  `phimint update`.
- `src/shell/wire.rs` — the startup check uses the same race.
- `src/model/config.rs` — the tips endpoint fix; the update endpoint default
  is replaced by the catalogue.
- `src/tips/mod.rs` — the tips race.
- `install.sh`, `install.ps1` — the installer's concurrent probe.
- `tests/` — the new tests.

## Known limits

- The 5-minute worst case for a *slow but working* primary mirror is accepted.
  The rate floor only catches a stalled link, not a merely-slow one — a link
  above 32 KiB/s is allowed to run to the 300s timeout. Making that faster
  means measuring throughput and risking abandoning a working mirror.
- The race adds one extra concurrent request per check. The update check runs
  at most once per interval and the tips fetch at most once per day.
- A user whose network blocks *both* mirrors gets no update. That is already
  the case today.
