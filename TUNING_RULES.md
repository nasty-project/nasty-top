# nasty-top Tuning Hints

These rules are evaluated on every configured refresh interval. The highest-severity non-dismissed finding produces a footer hint; within a severity, target findings precede the legacy tuning rules below. **Hints are not applied automatically.** Some offer an example sysfs command; target constraints and GC correlations offer investigation guidance instead. Press `N` to mute the current hint for 2 minutes or `!` to suppress it for the remainder of the run. Dismissals are scoped to the filesystem and diagnostic identity. Dismissing a finding allows the next finding to appear.

The heuristics are best-effort and unverified upstream; treat them as pointers to *something is happening*, not authoritative tuning advice.

A hint persists for at least 15 seconds after first appearing, even if the trigger condition clears in the same interval — so single-tick triggers don't flash by faster than you can read.

Rules are implemented in `src/advisor.rs` and `src/targets.rs`.

## Target Capacity / GC Pressure

Press **`v`** to show all target findings and supporting measurements; use the normal scroll keys to inspect large pools. The Background section shows the number of target findings even when a footer hint is muted. Muting affects footer hints, not the detailed evidence view.

Targets come from `metadata_target`, `foreground_target`, `background_target` and `promote_target`. Dot-delimited label membership is respected (`ssd.nvme` includes `ssd.nvme.0`, not `ssd.nvme2`). Device paths, including by-id aliases, are resolved to block names. Missing labels or unresolved aliases make membership unknown rather than an empty target.

For new allocation, members must be online, rw, allow the relevant data type, and not have durability=0. Unknown eligibility prevents capacity conclusions. Raw capacity is `nbuckets * bucket_size` for the filesystem member, not the whole parent disk. Per-member `alloc_debug` provides free buckets, btree sectors and fragmented sectors. Btree/fragmentation sectors are converted using 512 bytes per sector; free space uses the free-bucket count times bucket size.

| Condition | Severity | Finding |
|-----------|----------|---------|
| A configured target has no eligible members, with membership/eligibility known | Critical | Check labels, member state, durability and allowed data types |
| A metadata target has fewer eligible durability=1 members than requested replicas | Critical | Requested copies need more distinct writable members within the target |
| Reported physical btree footprint exceeds total eligible metadata-target capacity | Warning | Current footprint exceeds the target even before journals, reserves and other data; expand/select a larger healthy SSD target |
| Aggregate capacity fits but distinct-member capacity cannot fit an estimated uniform replica layout | Warning | Unequal member sizes may constrain placement; verify replica accounting |
| A member's kernel calculated copygc wait is non-positive | Warning | Name the pressured member and show GC state, free buckets and fragmentation |

### Replica-layout estimate

The physical btree footprint already includes copies; it is **not multiplied by metadata_replicas again**. The unequal-size estimate runs only with known unit durability, known capacities, enough members, and a reported zero metadata replica backlog. Let `P` be the physical footprint and `r` requested replicas. Estimate logical data `L = ceil(P/r)`, and check whether `sum(min(member_capacity, L)) < P`. Each member can hold at most one copy of a given block; excess capacity on one member cannot replace space for a separate replica. More than `r` members can distribute those copies, so this is not simply a smallest-device check.

This estimate assumes uniform replication. It does not measure existing over-replication or model failure domains, journals, reserves, fragmentation, or other data occupying the target. A footprint comparison describes today's reported storage, not an immutable requirement after compaction or policy changes. Findings explicitly qualify these limits. Targets allow spillover; a target constraint is not necessarily filesystem-wide ENOSPC.

### Sampling and missing data

Allocator tables and the physical footprint are sampled together every 10 seconds, or sooner on observed membership/state/options changes. Member eligibility and GC status are sampled each tick. Missing fields stay unknown (`?`) and a failed allocator refresh drops the old values. Truncated GC output only contributes the members actually parsed. The sign of the GC metric is used without pretending it is free bytes; negative values indicate GC is needed now, not negative free space.

Reconcile retains both data and metadata backlog columns. A `processing` parent with a closure wait is still reported as working; the tool does not infer a deadlock from that stack or from `POS_MIN`. GC pressure can explain delayed reconcile, but proving the worker dependency requires additional observations. Per-file target/replication overrides are not collected.

The parser fixtures in `src/fixtures/` include excerpts from the 66-device incident dump. `member-alloc-debug.txt` is synthetic, using the installed upstream buckets/sectors/fragmented format to verify units and truncation behavior.

## Legacy Tuning / Correlation Rules

| # | Condition | Proposal | Rationale |
|---|-----------|----------|-----------|
| 1 | Journal fill > 80% | Halve `journal_reclaim_delay` (min 10) | Journal is nearly full — reclaim space faster to prevent write stalls from journal exhaustion |
| 2 | Journal fill > 50% + watermark != "stripe" | Halve `journal_flush_delay` (min 100) | Journal filling with abnormal watermark — flush dirty entries more often to keep headroom |
| 3 | `blocked_journal_low_on_space` delta > 0 | Halve `journal_flush_delay` (min 100) | Actively blocking on journal space — flush more often |
| 4 | `blocked_write_buffer_full` delta > 0 | Halve `journal_flush_delay` (min 100) | Write buffer full stalls — flushing the journal more often frees buffer space |
| 5 | `blocked_allocate` delta > 0 | Increase `gc_reserve_percent` by 4 (max 20) | Allocator actively blocking — more GC reserve gives the allocator breathing room |
| 6 | Write stalls (last 60s) + `copygc_enabled=1` + copygc active | Inspect member headroom in Targets (`v`); no command suggested | GC may be recovering space required by allocation. Coincidence with stalls does not establish that disabling GC would help |

## Stall Detection

Stalls are detected from bcachefs `time_stats` "recent" (EWMA) mean, only when there is active IO:

- `data_read` recent mean > **200ms** with active reads → read stall
- `data_write` recent mean > **200ms** with active writes → write stall
- `btree_node_read` recent mean > **50ms** with new btree reads → metadata stall
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
