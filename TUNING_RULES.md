# nasty-top Tuning Hints

These rules are evaluated on every configured refresh interval. The highest-severity non-dismissed active finding produces a footer hint; within a severity, windowed diagnostics precede snapshot target/GC observations. Findings include investigation guidance rather than blanket sysfs changes. Press `N` to mute the current hint for 2 minutes or `!` to suppress it for the remainder of the run. Dismissals are scoped to the filesystem and diagnostic identity. Dismissing a finding allows the next finding to appear.

The heuristics are best-effort and unverified upstream; treat them as pointers to *something is happening*, not authoritative tuning advice.

A hint may remain displayed for 15 seconds after first appearing. A resolved or stale windowed finding is explicitly relabelled during that display retention. Snapshot hints no longer matching are labelled "Last observed". Display retention is separate from the duration of supporting evidence, and higher-priority findings can replace the displayed hint.

Rules are implemented in `src/diagnostics.rs`, `src/advisor.rs`, `src/targets.rs`, `src/peers.rs` and `src/trends.rs`.

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

## Peer-Aware Latency

The device table no longer compares all devices against one pool-wide latency median. Peer selection:

1. Choose the narrowest configured target containing the member. When none resolves for that member, use its parent label group; explicitly unlabelled members can use a media-only unlabelled group. A singleton target is not widened just to manufacture peers.
2. Resolve the block topology through partition parents and `slaves` links. Use the reported media of backing leaves: rotational, NVMe, or other non-rotational. A dm queue reporting non-rotational does not override rotational leaves. Mixed/incomplete/cyclic graphs are unknown. Resolution is cached per collection and bounded to 16 levels, 4096 nodes and 256 leaves.
3. Require online members, the same media class and backing-leaf count. Exclude the candidate and any peer sharing backing leaves with it or another selected peer. This prevents multiple partitions/mappers sharing a visible backing device becoming independent references. Overlap uses kernel-device names: multipath aliases and hardware resources hidden by a controller may require manual interpretation.
4. Evaluate reads and writes separately. Require at least 5 completed operations per member per sample and at least **two comparable peers**. Directional IOPS must be within a factor of four; average queues within a factor of two (with a one-request floor); read fractions within 25 percentage points.
5. A latency deviation exceeds `max(3 * peer median await, media floor)`. Floors are **20ms rotational**, **2ms NVMe**, and **5ms other non-rotational**. The candidate is excluded from the median. Request size/locality and other users of the backing device are not modeled.

The table can mark a deviation immediately. An Advisor finding requires **30 seconds and at least three consecutive valid observations**. Missing peers, changed topology/group, reset counters or sampling gaps break continuity. While a deviation is warming up after missing data, a previous finding remains stale rather than being declared resolved. A valid non-outlier sample resolves it.

Findings show the group, selected peer names, await and AQ comparisons, and optionally the device's own earlier comparable/non-outlier baseline. That baseline is request-weighted, requires at least 30 seconds of history, and uses bounded/coalesced samples from the preceding five minutes. Outlier samples do not update the reference. Concurrent error-counter increases are noted without claiming a hardware cause.

The `!` marker also preserves direct queue pressure: `Q > 4`, or valid interval statistics with `AQ >= 2`, or utilization at least 95% with await at least 20ms. These conditions can flag a mapped-device queue even when no valid peers exist. **`!` is a pressure marker, not an error count or a failed-disk indicator.** `Q` is instantaneous; `AQ` is time-averaged outstanding requests. See the README device-table legend.

## Target Headroom Trends

Each target role records up to **10 minutes / 61 samples**, accepting at most one fresh allocator sample per 10 seconds. Duplicate allocator timestamps are ignored, including their cached pressure/backlog context. Policy, membership, eligible capacity changes, unavailable measurements and collection gaps restart the history. Unknown pressure is retained as unknown.

The Targets view reports net direction over the window, the first/latest free-bucket values, actual coverage, fresh sample count and sample age. It also reports GC pressure and running-copygc sample counts, plus placement backlog for metadata and background targets where available. Targets may overlap; their capacities and trends must not be added together as independent pools.

A falling-headroom warning requires all of:
- At least **120 seconds** of observations.
- A net loss of at least **max(64 MiB, 10% of starting free buckets)**.
- At least three decreasing intervals, with decreases comprising at least 70% of non-flat intervals.
- A further decline relative to a sample at least 30 seconds before the latest sample (a one-off drop followed by a plateau does not warn).
- GC pressure recorded in at least **80% of all samples**, including unknown samples in the denominator.

Direction is descriptive net movement, not a forecast. A continued decline without enough GC-pressure evidence is shown in the history but does not trigger this warning. A valid window no longer meeting the rule resolves the finding; an unavailable or rebuilding window leaves it stale. Free buckets are not guaranteed allocatable space, and no time-until-full estimate is produced.

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
- PCIe AER counter monitoring correlated with filesystem members.
