# Tier 2 V8 Heap-Cap Experiment

Date: 2026-07-15
Platform: macOS, release `draco` binary built from the current worktree
Sites: `https://thrill.com` and `https://bluff.com`

## Method

An experimental `DRACO_CAPTURE_MAX_HEAP_MB` override was clamped to 128–192 MiB
and passed directly to V8's `CreateParams::heap_limits`. The near-heap callback
was unchanged. Each measurement launched a fresh `draco scrape --json
--runtime-log` process under `/usr/bin/time -l`; runs were strictly sequential.

Three runs per site were made at 192, 176, 160, 144, and 128 MiB (30 measured
runs). The harness required:

- process exit 0 and JSON `status=success`;
- Markdown length greater than 40,000 for Thrill and 20,000 for Bluff;
- Thrill runtime logs containing both `games-state.thrill.com/snapshots/` and
  `/api/v2/games/providers`, or Bluff logs containing `/promotions`;
- `[raze.window] closed via quiesce`;
- all six `[raze.memory]` phase records; and
- no `V8 heap limit reached` diagnostic.

`runtime_ms` is the runtime's own final `[raze.phases]` value. It is unavailable
when the heap guard terminates the isolate before that final record. `wall_s` and
`max_rss_bytes` come from `/usr/bin/time -l`. All 30 processes exited 0 and
returned the outer JSON status `success`; the semantic column catches the
important inner runtime failures that those envelope fields alone do not expose.

## Raw results

| Cap MiB | Run | Site | Runtime ms | Wall s | Max RSS bytes | Markdown chars | Endpoints | Quiesced | Memory phases | Heap terminated | Semantic |
|---:|---:|:---|---:|---:|---:|---:|:---:|:---:|---:|:---:|:---:|
| 192 | 1 | Thrill | 4531 | 4.85 | 301776896 | 44716 | pass | pass | 6 | no | pass |
| 192 | 1 | Bluff | 3925 | 6.01 | 300417024 | 98692 | pass | pass | 6 | no | pass |
| 192 | 2 | Thrill | 4015 | 4.31 | 301924352 | 44716 | pass | pass | 6 | no | pass |
| 192 | 2 | Bluff | 2153 | 3.05 | 250085376 | 24865 | pass | pass | 6 | no | pass |
| 192 | 3 | Thrill | 4248 | 4.56 | 301056000 | 44716 | pass | pass | 6 | no | pass |
| 192 | 3 | Bluff | 2113 | 3.87 | 250265600 | 24865 | pass | pass | 6 | no | pass |
| 176 | 1 | Thrill | — | 4.09 | 273956864 | 0 | fail | fail | 4 | yes | fail |
| 176 | 1 | Bluff | 2170 | 4.03 | 242499584 | 24865 | pass | pass | 6 | no | pass |
| 176 | 2 | Thrill | — | 3.71 | 275939328 | 0 | fail | fail | 4 | yes | fail |
| 176 | 2 | Bluff | 1652 | 2.60 | 242728960 | 24865 | pass | pass | 6 | no | pass |
| 176 | 3 | Thrill | — | 4.43 | 279150592 | 0 | fail | fail | 4 | yes | fail |
| 176 | 3 | Bluff | 2161 | 3.32 | 240795648 | 24865 | pass | pass | 6 | no | pass |
| 160 | 1 | Thrill | — | 3.99 | 264454144 | 0 | fail | fail | 4 | yes | fail |
| 160 | 1 | Bluff | 2178 | 3.97 | 234225664 | 24865 | pass | pass | 6 | no | pass |
| 160 | 2 | Thrill | — | 3.98 | 264404992 | 0 | fail | fail | 4 | yes | fail |
| 160 | 2 | Bluff | 2109 | 3.95 | 234520576 | 24865 | pass | pass | 6 | no | pass |
| 160 | 3 | Thrill | — | 3.86 | 264388608 | 0 | fail | fail | 4 | yes | fail |
| 160 | 3 | Bluff | 2107 | 3.99 | 235470848 | 24865 | pass | pass | 6 | no | pass |
| 144 | 1 | Thrill | — | 4.10 | 255295488 | 0 | fail | fail | 4 | yes | fail |
| 144 | 1 | Bluff | 2255 | 4.01 | 225968128 | 24865 | pass | pass | 6 | no | pass |
| 144 | 2 | Thrill | — | 3.59 | 254492672 | 0 | fail | fail | 4 | yes | fail |
| 144 | 2 | Bluff | 2162 | 4.07 | 225280000 | 24865 | pass | pass | 6 | no | pass |
| 144 | 3 | Thrill | — | 4.20 | 255442944 | 0 | fail | fail | 4 | yes | fail |
| 144 | 3 | Bluff | 2216 | 4.00 | 225656832 | 24865 | pass | pass | 6 | no | pass |
| 128 | 1 | Thrill | — | 4.24 | 233488384 | 0 | fail | fail | 4 | yes | fail |
| 128 | 1 | Bluff | 2169 | 4.00 | 202850304 | 24865 | pass | pass | 6 | no | pass |
| 128 | 2 | Thrill | — | 4.40 | 230883328 | 0 | fail | fail | 4 | yes | fail |
| 128 | 2 | Bluff | 1643 | 2.59 | 203948032 | 24865 | pass | pass | 6 | no | pass |
| 128 | 3 | Thrill | — | 3.34 | 234422272 | 0 | fail | fail | 4 | yes | fail |
| 128 | 3 | Bluff | 1655 | 2.51 | 204128256 | 24865 | pass | pass | 6 | no | pass |

## Summary

| Cap MiB | Site | Semantic passes | Median runtime ms | Max runtime ms | Median RSS bytes | Max RSS bytes |
|---:|:---|---:|---:|---:|---:|---:|
| 192 | Thrill | 3/3 | 4248 | 4531 | 301776896 | 301924352 |
| 192 | Bluff | 3/3 | 2153 | 3925 | 250265600 | 300417024 |
| 176 | Thrill | 0/3 | — | — | 275939328 | 279150592 |
| 176 | Bluff | 3/3 | 2161 | 2170 | 242499584 | 242728960 |
| 160 | Thrill | 0/3 | — | — | 264404992 | 264454144 |
| 160 | Bluff | 3/3 | 2109 | 2178 | 234520576 | 235470848 |
| 144 | Thrill | 0/3 | — | — | 255295488 | 255442944 |
| 144 | Bluff | 3/3 | 2216 | 2255 | 225656832 | 225968128 |
| 128 | Thrill | 0/3 | — | — | 233488384 | 234422272 |
| 128 | Bluff | 3/3 | 1655 | 2169 | 203948032 | 204128256 |

## Decision

Keep the default at **192 MiB**.

Every lower cap failed the first promotion gate: all three Thrill runs terminated
at the V8 heap guard, produced no Markdown, missed both endpoint assertions, did
not quiesce, and emitted only four of six memory phases. Therefore runtime and
RSS improvements on Bluff cannot make any lower cap eligible. In particular,
176 MiB saved only 7,766,016 median RSS bytes on Bluff (about 7.4 MiB, below the
10 MiB improvement gate) and still failed Thrill. Caps at 160 MiB and below saved
more Bluff RSS but failed Thrill deterministically.

No lower cap was a contender after the initial three-run screen, so none was
expanded to five runs. The environment override was only benchmark scaffolding;
it and its parsing tests were removed after measurement. Production retains the
fixed 192 MiB default and an exact unit test for that value.

## Final fixed-default verification

After removing the override and rebuilding the release CLI, a final sequential
`bash tests/profile_spa_memory.sh` run passed both semantic profiles at the fixed
192 MiB default:

| Site | Markdown chars | Max RSS bytes | Endpoint gates | Quiesced | Memory phases |
|:---|---:|---:|:---:|:---:|---:|
| Thrill | 44716 | 300548096 | pass | pass | 6 |
| Bluff | 24992 | 252837888 | pass | pass | 6 |
