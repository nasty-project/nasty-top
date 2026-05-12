# nasty-top

A top-like TUI for bcachefs filesystems. Real-time per-device IO, latency, and internal stats with built-in tuning advisor.

Built for [NASty](https://github.com/nasty-project/nasty) but works on any system with a mounted bcachefs filesystem.

![nasty-top device IO](screen1.jpg)
![nasty-top process IO](screen2.jpg)
![nasty-top counters](screen3.jpg)

## Features

- **Live IO throughput and latency** per device with user/btree/journal/sb breakdown
- **Latency from bcachefs time_stats** (EWMA rolling mean, not useless cumulative averages)
- **Per-device IO breakdown** using `io_done` JSON and `io_latency_stats_*_json`
- **Blocked stats view** showing what's actually blocking IO right now (allocator, journal, write buffer, etc.)
- **Stall detection** with 60-second event log when latency exceeds 200ms
- **Tuning hints** that flag known bcachefs pressure signals and show an example sysfs command (informational only — not applied automatically)
- **Options panel** with inline editing of runtime-tunable sysfs options
- **Multi-filesystem support** — press `f` to cycle between mounted bcachefs filesystems
- **Process IO view** showing which processes are doing IO
- **Journal fill %**, load average, reconcile progress
- **Consistent color scheme**: yellow = read, blue = write, red = errors/stalls

## Install

**Nix:**
```bash
nix run github:nasty-project/nasty-top
```

**Homebrew (Linux):**
```bash
brew install fenio/tap/nasty-top
```

**Download binary:**
```bash
curl -sL https://github.com/nasty-project/nasty-top/releases/latest/download/nasty-top-x86_64-linux.tar.gz | \
  sudo tar xzf - -C /usr/local/bin/
```

**Build from source:**
```bash
cargo install --path .
# or cross-compile from macOS:
brew install filosottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl
./deploy.sh root@your-nas
```

## Usage

```
nasty-top [OPTIONS]

Options:
  -f, --filesystem <NAME|UUID>  Filesystem to monitor (default: first found)
  -t, --interval <SECONDS>      Refresh interval (default: 2)
  -h, --help                    Print help
```

## Keybindings

| Key | Action |
|-----|--------|
| `?` | Toggle help popup |
| `o` | Toggle options panel (hidden by default) |
| `c` | Toggle counters view |
| `t` | Toggle blocked stats / time_stats view |
| `p` | Toggle process IO view |
| `r` | Toggle reconcile on/off |
| `g` | Toggle copygc on/off |
| `f` | Cycle between filesystems |
| `Tab` | Switch focus between metrics and options panel |
| `↑`/`k`, `↓`/`j` | Navigate options / scroll counter, blocked, process views |
| `Enter` | Edit selected option value (in options panel) |
| `Esc` | Cancel edit / dismiss status message |
| `N` | Mute current hint for 2 minutes |
| `!` | Never show this hint again |
| `C` | Clear all permanent mutes |
| `q` / `Ctrl-C` | Quit |

## Data Sources

| Metric | Source | Notes |
|--------|--------|-------|
| IO throughput | `dev-N/io_done` (JSON) | Per-type breakdown, diffed per tick |
| IO latency (device) | `dev-N/io_latency_stats_{r,w}_json` | EWMA mean, shown only when active |
| IO latency (fs) | `time_stats/data_{read,write}` | "recent" column rolling mean |
| Blocked stats | `time_stats/blocked_*` | Count delta per tick + recent mean |
| Journal fill | `internal/journal_debug` | dirty/total entries + watermark |
| Reconcile | `bcachefs reconcile status` | Subprocess, parsed for progress |
| Process IO | `/proc/<pid>/io` | read_bytes/write_bytes diffed |
| Options | `options/*` | Read/write directly to sysfs |

## Tuning Hints

When known bcachefs pressure signals fire (journal fill, blocked allocator, etc.), a hint appears in the footer with a short reason and an example sysfs command you *could* run. Nothing is applied automatically — the hint is informational. The current heuristics are unverified; treat them as a starting point for investigation, not a prescription. See [TUNING_RULES.md](TUNING_RULES.md) for the full rule set.

## License

GPL-3.0-only
