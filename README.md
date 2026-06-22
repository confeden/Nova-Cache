# Nova Cache v0.9

> A kernel-level disk caching system for Windows that accelerates HDD read performance by caching hot data in RAM and SSD tiers.

## Overview

Nova Cache uses a **Windows Minifilter Driver** (C, WDK) and a **user-mode service** (Rust, tokio) to implement transparent three-tier disk caching:

| Tier | Medium | Speed | Purpose |
|------|--------|-------|---------|
| **L1** | RAM (shared memory) | Nanoseconds | Fastest tier — hot data |
| **L2** | SSD (memory-mapped file) | Microseconds | Warm data — persistent across reboots |
| **HDD** | Physical disk | Milliseconds | Cold reads — cache misses only |

The driver intercepts every `ReadFile`/`WriteFile` call at the kernel level, checks L1 and L2 directories in shared memory, and serves hits directly — bypassing the physical disk. Cache misses and writes are forwarded to the service via a ring buffer, which then promotes data into L1/L2 using an **ARC** (Adaptive Replacement Cache) policy with **TinyLFU** admission filter.

## Features

- **Transparent caching** — no drive letters, mount points, or manual configuration required
- **3-tier architecture** — RAM (L1) → SSD (L2) → HDD (origin), with automatic promotion
- **Adaptive replacement (ARC + TinyLFU)** — self-tuning eviction that adapts to workload
- **Write-back caching** — coalesces writes and flushes asynchronously to the backing disk
- **Adaptive prefetch** — ETW-driven read pattern detection with variable window (1–64 MB)
- **HDD read coalescing** — merges sequential reads into fewer syscalls to reduce seek overhead
- **Memory-mapped L2** — zero-copy I/O via `memmap2` for minimal latency
- **Per-process shared memory** — per-PID sections for isolation
- **Performance monitoring** — real-time stats via IPC including hit rate, latency, and boost multiplier
- **Graphical monitor** — egui dashboard for metrics and configuration

## Quick Start

> **Windows 10+ x64** only. Requires Administrator privileges.

Open **Command Prompt as Administrator** and run:

```cmd
dev.bat
```

That's it. The script will:
1. Install the **Rust toolchain** if missing (via [rustup](https://rustup.rs))
2. Detect or install **Visual Studio Build Tools** with C++ workload and **WDK** (for driver building)
3. Build the **kernel driver** (`Novacache.sys`) via MSBuild
4. Build the **Rust service** via `cargo`
5. Create a **self-signed test certificate** and sign the driver
6. Start `nova-cache-service`

All steps are **idempotent** — completed steps are skipped on subsequent runs.

```cmd
dev.bat --force    # force rebuild all
```

## Configuration

After the first run, edit `config/nova_cache.toml` to configure caching:

### L1 Cache (RAM)

L1 is enabled by default with **512 MB** of shared memory. Adjust in the config:

```toml
[cache]
l1_size_mb = 512    # increase if you have more RAM available
```

### L2 Cache (SSD)

L2 is **disabled by default**. To enable SSD caching:

1. Edit `config/nova_cache.toml`:
   ```toml
   [cache.l2]
   enable = true
   path = 'X:\l2_cache.dat'    # path on an SSD (NOT your OS drive)
   size_gb = 50                 # size in GB
   ```
2. Restart the service: `dev.bat`

> **Tip:** Use a dedicated SSD or a separate partition for L2. Do not use your OS drive — it defeats the purpose of caching.

### Cached Volumes

By default, **no volumes are cached**. To enable caching for a drive:

1. Edit `config/nova_cache.toml` and add a volume:
   ```toml
   [[volumes]]
   volume = "D"        # drive letter (no colon)
   enabled = true
   ```
2. Restart the service: `dev.bat`

Or use the GUI dashboard to add/remove volumes interactively.

### Game Mode

Game mode boosts cache priority for game/application processes:

```toml
[game_mode]
enabled = true
priority_boost = 2.0    # multiply cache priority for detected games
detect_games = true     # auto-detect game processes
```

## Repository Structure

```
crates/
├── nova-cache-core         — Core caching logic (ARC, TinyLFU, L1/L2/HDD backends)
├── nova-cache-service      — Windows service (tokio async runtime, IPC, flush thread)
├── nova-cache-gui          — Graphical monitor (egui)
├── nova-cache-monitor      — Prefetch engine, stats collection
├── nova-cache-driver-comm  — Shared memory protocol definitions
├── nova-cache-kdu          — KDU driver loading interop
├── nova-bench              — Benchmarking tool
└── nova-stress             — Stress testing tool
driver/
└── novacache               — Minifilter driver (C, WDK)
config/                     — Configuration files
scripts/                    — Build and management scripts
```

## License

GNU General Public License v3.0 — see [LICENSE](LICENSE).

## Attribution

Built with:
- [Windows Kernel-Mode Driver Framework (WDK)](https://learn.microsoft.com/en-us/windows-hardware/drivers/)
- [Rust](https://www.rust-lang.org/) and [tokio](https://tokio.rs/)
- [egui](https://github.com/emilk/egui) — immediate mode GUI
- [KDU](https://github.com/hfiref0x/KDU) — kernel driver utility
