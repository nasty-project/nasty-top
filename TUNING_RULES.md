# nasty-top Tuning Hints

These rules are evaluated on every tick (2s), in priority order. The first matching rule produces an informational hint shown in the footer bar with an example sysfs command. **Hints are not applied automatically** — the user decides whether to run the command. Press `N` to mute the current hint for 2 minutes or `!` to suppress it permanently.

The heuristics are best-effort and unverified upstream; treat them as pointers to *something is happening*, not authoritative tuning advice.

A hint persists for at least 15 seconds after first appearing, even if the trigger condition clears in the same interval — so single-tick triggers don't flash by faster than you can read.

Rules are implemented in `src/advisor.rs`.

## Active Rules

| # | Condition | Proposal | Rationale |
|---|-----------|----------|-----------|
| 1 | Journal fill > 80% | Halve `journal_reclaim_delay` (min 10) | Journal is nearly full — reclaim space faster to prevent write stalls from journal exhaustion |
| 2 | Journal fill > 50% + watermark != "stripe" | Halve `journal_flush_delay` (min 100) | Journal filling with abnormal watermark — flush dirty entries more often to keep headroom |
| 3 | `blocked_journal_low_on_space` delta > 0 | Halve `journal_flush_delay` (min 100) | Actively blocking on journal space — flush more often |
| 4 | `blocked_write_buffer_full` delta > 0 | Halve `journal_flush_delay` (min 100) | Write buffer full stalls — flushing the journal more often frees buffer space |
| 5 | `blocked_allocate` delta > 0 | Increase `gc_reserve_percent` by 4 (max 20) | Allocator actively blocking — more GC reserve gives the allocator breathing room |
| 6 | Write stalls (last 60s) + `copygc_enabled=1` + copygc active | Set `copygc_enabled=0` | Background copy-GC is competing with foreground writes |

## Stall Detection

Stalls are detected from bcachefs `time_stats` "recent" (EWMA) mean, only when there is active IO:

- `data_read` recent mean > **200ms** with active reads → read stall
- `data_write` recent mean > **200ms** with active writes → write stall
- `btree_node_read` recent mean > **50ms** → metadata stall
- Journal dirty entries jump by >1000 in one tick AND fill >70% → journal pressure

Stall events expire after **60 seconds**. Up to 10 are tracked, last 5 shown in the Background section.

## Blocked Stats

The `time_stats/blocked_*` entries are the most precise bottleneck indicators. The advisor uses **per-tick deltas** (not cumulative counts) to detect active blocking:

| Stat | What it means |
|------|---------------|
| `blocked_allocate` | Waiting for free space from the allocator |
| `blocked_allocate_open_bucket` | Waiting for an open write bucket |
| `blocked_journal_low_on_space` | Journal running out of space |
| `blocked_journal_max_in_flight` | Too many journal writes in flight |
| `blocked_write_buffer_full` | Write buffer saturated |
| `blocked_writeback_throttle` | Writeback pressure from the kernel |
| `blocked_key_cache_flush` | Key cache flush contention |

## Future Rule Ideas

- Write stalls while rebalance is active → throttle via `move_bytes_in_flight` / `move_ios_in_flight` (needs empirical validation; the older `rebalance_enabled` knob no longer exists upstream)
- High `blocked_journal_max_in_flight` rate → reduce concurrent writers or increase journal size
- Read amplification (btree reads >> user reads) → suggest larger btree node size (mount-time only)
- Device with significantly higher latency than others → flag potential hardware issue
- High write latency + low compression ratio → suggest switching to lz4 or none
