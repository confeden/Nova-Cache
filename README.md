# Nova Cache

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

## Building

### Requirements

- **Windows 10+ x64** with WDK (for the driver)
- **Rust toolchain** (stable) — install via [rustup.rs](https://rustup.rs)
- **Visual Studio** with "Desktop development with C++" workload (for WDK/msbuild)

### Steps

```powershell
# Build all Rust crates
cargo build --release

# Build the kernel driver
msbuild driver\novacache\Novacache.vcxproj /p:Configuration=Release /p:Platform=x64
```

The output driver binary will be at `driver\novacache\Release\Novacache.sys`.

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

## Loading the Driver

> ⚠ **Nova Cache is designed for development/testing environments.** The driver is not digitally signed for public release and requires disabling Driver Signature Enforcement (DSE) or using a kernel driver utility.

1. Build the driver and service as described above.
2. Disable DSE temporarily (requires admin):
   ```powershell
   .\scripts\load_driver.ps1
   ```
3. The service auto-loads: `nova-cache-service.exe`

## Attribution

Built with:
- [Windows Kernel-Mode Driver Framework (WDK)](https://learn.microsoft.com/en-us/windows-hardware/drivers/)
- [Rust](https://www.rust-lang.org/) and [tokio](https://tokio.rs/)
- [egui](https://github.com/emilk/egui) — immediate mode GUI
- [KDU](https://github.com/hfiref0x/KDU) — kernel driver utility
