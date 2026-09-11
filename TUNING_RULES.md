# nasty-top Tuning Hints

These rules are evaluated on every configured refresh interval. The highest-severity non-dismissed active finding produces a footer hint; within a severity, windowed diagnostics precede snapshot target/GC observations. Findings include investigation guidance rather than blanket sysfs changes. Press `N` to mute the current hint for 2 minutes or `!` to suppress it for the remainder of the run. Dismissals are scoped to the filesystem and diagnostic identity. Dismissing a finding allows the next finding to appear.

The heuristics are best-effort and unverified upstream; treat them as pointers to *something is happening*, not authoritative tuning advice.

A hint may remain displayed for 15 seconds after first appearing. A resolved or stale windowed finding is explicitly relabelled during that display retention. Snapshot hints no longer matching are labelled "Last observed". Display retention is separate from the duration of supporting evidence, and higher-priority findings can replace the displayed hint.

Rules are implemented in `src/diagnostics.rs`, `src/advisor.rs` and `src/targets.rs`.

## Evidence Windows and Advisor View

Press **`a`** to inspect all findings, including muted hints and recently resolved/stale diagnoses. The view distinguishes **observed facts** from **possible causes**, and shows supporting measurements, first/last observation times, status age and the next investigation step.

- Counter deltas use monotonic timestamps over **up to 60 seconds**. Each measurement reports its actual coverage and rate; shorter startup windows are not presented as full-minute measurements.
- Sub-second observations are coalesced into approximately one-second buckets to bound memory at high refresh rates. Intervals crossing the window boundary are discarded rather than inventing a prorated event count.
- The maximum accepted collection gap is `min(max(3 * configured_interval, 5s), 60s)`. Larger gaps and non-monotonic timestamps reset evidence continuity. Very sparse sampling cannot establish sustained conditions.
- A filesystem collection taking longer than that limit is treated as stale, rather than timestamping old readings as newly collected. Block progress uses the separate timestamp from the common `/proc/diskstats` read.
- A counter reset discards that counter's window. Missing counters are unknown, not zero; when restored, they establish a new baseline.
- Device history uses filesystem and member UUIDs. If a member UUID is unavailable, index/name are used; replacement between observations cannot always be distinguished in that fallback mode. Disappearing members lose their baselines. Switching filesystems clears the selected diagnostic history.
- **Active** means the rule is supported by its current observation/window. For error counters, this can include an increase earlier in the window, not necessarily this tick.
- **Resolved** means valid observations no longer satisfy the rule. This does not certify that an underlying hardware problem was repaired.
- **Stale** means required observations are unavailable/reset or continuity was lost. It never means healthy.
- Resolved/stale history expires 60 seconds after the last active observation, with a maximum of 128 recent records. Only active findings participate in new footer selection.

### Outstanding I/O

Flag a member when its queue remains nonzero across at least three consecutive valid observations and no tracked completion counter advances for **30 seconds**. Completion progress, a drained queue, missing/old data, counter reset, or an accounting-format change breaks the streak.

Reads, writes, discards and flushes are considered where available. Linux does not account flush completions to partitions, so that field is marked unavailable for partition members. Findings explicitly state completion visibility and say "no observed completions"; they do not claim to identify a request or measure its exact age. A stuck request surrounded by other successful completions cannot be isolated with these aggregate counters.

### Device Errors and State

Initial error counts establish history, not a new-error alarm. Later increases report each available category and its actual observation duration/rate. The current upstream `io_errors` file contains both lifetime and since-reset sections; only lifetime counters are used. Flush-honoring errors are retained as a separate category. This also corrects double-counting in the device table's Err totals.

Observed transitions to a non-rw state or offline status produce a critical finding, retained while valid samples continue to show that condition. Recovery to rw/online resolves it and records recovery evidence. An already read-only device on first observation is a baseline, not a new transition. Administrative changes can also produce these transitions; they are not proof of failed hardware.

Increases on at least two members within the evidence window produce a grouped possible-cause finding suggesting inspection of shared power, cabling, backplanes and controllers. This does not establish that failures were simultaneous or had a common cause.

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

## Journal / Allocator Classification

Journal classifications share one finding ID, evaluated in the following order. Slow operations require a recent EWMA of at least **200ms**, captured when their operation count advances. An idle, stale EWMA cannot create a completion/reclaim diagnosis. JSON stats have text fallbacks for the journal operations on older modules; known zero counts are retained.

| Condition in the evidence window | Finding | Interpretation / next step |
|----------------------------------|---------|----------------------------|
| `blocked_journal_low_on_space` increases | Journal-space pressure (observed) | Inspect reclaim and allocation headroom |
| Open-entry/in-flight blocking plus slow active btree/key-cache pin flushes | Possible metadata-reclaim bottleneck | Inspect metadata writeback and device latency; correlation does not prove the dependency |
| In-flight blocking plus slow active journal write/sequence flushes | Possible journal-completion bottleneck | Inspect outstanding work and flush latency; in-flight blocking alone does not justify a larger journal |
| Open-entry or in-flight blocking without the above evidence | Journal-pipeline pressure (observed) | Inspect outstanding work and dependencies; cause remains undetermined |

Each contributing counter reports its own valid coverage. Missing space-block counters are shown as unavailable, never as evidence that space blocking did not occur. Current/on-disk journal sequence positions and observed sequence advances are included where available.

Additional observations:

- **Write-buffer pressure:** increasing `blocked_write_buffer_full`; investigate btree-buffer flushing and metadata pressure.
- **Allocator blocking:** increasing `blocked_allocate`; investigate member free buckets, reserves and GC pressure. Raising the reserve does not create physical capacity.
- **High dirty-entry occupancy:** dirty/total journal entries exceed 80%. This is explicitly entry occupancy, **not on-disk journal-space utilization**; the System gauge is labelled `JEnt` and displays `?` if unavailable. Occupancy alone does not establish a bottleneck or justify changing flush delays.
- **Write stalls with copygc active:** existing snapshot correlation directs users to member headroom in Targets (`v`). GC may be recovering space required by allocation, so disabling it is not recommended automatically.

## Stall Detection

Stalls are detected from bcachefs `time_stats` "recent" (EWMA) mean, only when there is active IO:

- `data_read` recent mean > **200ms** with active reads → read stall
- `data_write` recent mean > **200ms** with active writes → write stall
- `btree_node_read` recent mean > **50ms** with new btree reads → metadata stall
- Journal dirty entries jump by >1000 in one tick AND occupancy >70% → entry-occupancy observation (not disk-space exhaustion)

Stall events expire after **60 seconds**. Up to 10 are tracked, last 5 shown in the Background section.

## Blocked Stats

The `time_stats/blocked_*` entries identify completed blocking events. The Blocked view shows **per-tick deltas**; the advisor aggregates valid deltas over its evidence window. An in-progress wait may not increment a counter until it finishes, so these counters alone cannot detect every current stall:

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

- Reconcile progress tracking, distinguishing scanning, movement and deliberately pending work.
- Tier-aware device latency comparisons and device-specific baselines.
- Sustained per-tier headroom trends.
- PCIe AER counter monitoring correlated with filesystem members.
