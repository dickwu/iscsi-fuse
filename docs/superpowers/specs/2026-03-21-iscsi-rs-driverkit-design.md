# iscsi-rs: macOS iSCSI Initiator via DriverKit

**Date:** 2026-03-21
**Status:** Approved
**Replaces:** iscsi-fuse (FUSE-based approach)

## Problem

The current `iscsi-fuse` project exposes iSCSI LUNs on macOS via FUSE + DiskImages (`hdiutil attach`). This architecture has fatal flaws:

1. **DiskImages driver never flushes writes to FUSE backing file** — data is silently lost
2. **APFS formatting fails** — APFS kernel internals bypass FUSE semantics
3. **`FOPEN_DIRECT_IO` breaks DiskImages entirely** — returns EINVAL on all I/O
4. **Write persistence bug** — data doesn't survive unmount/remount

Root cause: macOS has no mechanism to create `/dev/diskN` from userspace except DiskImages (broken for this use case) or DriverKit (the correct solution). Every working iSCSI initiator (Linux open-iscsi, Windows iSCSI) uses a kernel component for block device creation.

## Solution

Replace FUSE entirely with a DriverKit system extension (`IOUserBlockStorageDevice`) that creates a real `/dev/diskN`. Split into three components:

- **C++ DriverKit dext** — thin block device proxy (~500-800 LOC)
- **Rust daemon (`iscsid`)** — iSCSI protocol, caching, block layer (~85% code reuse)
- **Rust CLI (`iscsi-rs`)** — user interface for login/logout/status

## Architecture

```
┌─────────────────────────────────────────────────┐
│  macOS Kernel                                   │
│  ┌───────────────────────────────────────────┐  │
│  │ Filesystem (APFS/HFS+) → Block I/O layer │  │
│  └──────────────────┬────────────────────────┘  │
│                     │ doAsyncReadWrite()         │
│  ┌──────────────────▼────────────────────────┐  │
│  │  DriverKit Dext (C++, ~500-800 LOC)       │  │
│  │  IOUserBlockStorageDevice                 │  │
│  │  - Reports geometry (block size, count)   │  │
│  │  - Receives read/write/sync from kernel   │  │
│  │  - Forwards to daemon via IOUserClient    │  │
│  │  - Returns completions back to kernel     │  │
│  └──────────────────┬────────────────────────┘  │
└─────────────────────┼───────────────────────────┘
                      │ IOUserClient IPC
                      │ (shared memory for data,
                      │  ExternalMethod for control)
┌─────────────────────▼───────────────────────────┐
│  Rust Daemon (iscsid)                           │
│  ┌───────────────────────────────────────────┐  │
│  │ IOKit client (iokit-sys FFI)              │  │
│  │ - Opens IOUserClient to dext              │  │
│  │ - Maps shared memory buffers              │  │
│  │ - Receives block I/O requests             │  │
│  │ - Returns completions                     │  │
│  ├───────────────────────────────────────────┤  │
│  │ Block layer (existing, mostly reused)     │  │
│  │ - Cache (moka), readahead, coalescing     │  │
│  ├───────────────────────────────────────────┤  │
│  │ iSCSI stack (existing, fully reused)      │  │
│  │ - PDU, session, pipeline, recovery        │  │
│  └──────────────────┬────────────────────────┘  │
│                     │ TCP                        │
└─────────────────────┼───────────────────────────┘
                      ▼
               iSCSI Target (QNAP)
```

### Why This Architecture

The DriverKit sandbox **cannot open TCP sockets** (confirmed by Apple, blocked iSCSI-osx project migration). This forces a split design: dext handles block device, daemon handles networking. This is architecturally analogous to Linux open-iscsi (kernel data path + userspace control plane).

The split is actually beneficial:
- Dext is stable, rarely needs updates, survives daemon restarts
- All logic stays in Rust where development is productive
- Daemon can be updated independently without re-approving the dext
- Karabiner-DriverKit-VirtualHIDDevice validates this pattern in production with millions of users

## Target Platform

- macOS 15+ (Sequoia)
- Apple Silicon and Intel (universal binary)
- Requires Apple Developer account for distribution (DriverKit entitlements)

## Component Details

### DriverKit Extension (`com.peilinwu.iscsi-rs.dext`)

**Language:** C++

**Classes:**

```cpp
class ISCSIBlockStorageDevice : public IOUserBlockStorageDevice {
    kern_return_t Start(IOService* provider) override;
    kern_return_t Stop(IOService* provider) override;
    kern_return_t doAsyncReadWrite(IOMemoryDescriptor* buffer,
                                   uint64_t block, uint64_t nblks,
                                   IOStorageAttributes* attr,
                                   IOStorageCompletion* completion,
                                   bool isRead) override;
    kern_return_t doSynchronize() override;
    kern_return_t doEjectMedia() override;
    kern_return_t ReportMaxReadTransfer(uint64_t* blockCount) override;
    kern_return_t ReportMaxWriteTransfer(uint64_t* blockCount) override;
    kern_return_t ReportMediumWritable(bool* writable) override;
    kern_return_t ReportBlockSize(uint64_t* blockSize) override;
    kern_return_t ReportMaxBlockCount(uint64_t* blockCount) override;
    // Future: doUnmap() for TRIM/DISCARD support (APFS benefits from this)
};

// Diagnostics: use os_log for dext-side debugging (available in DriverKit sandbox).
// Logs appear in Console.app under the dext's subsystem identifier.

class ISCSIUserClient : public IOUserClient {
    kern_return_t ExternalMethod(uint64_t selector,
                                 IOUserClientMethodArguments* args,
                                 const IOUserClientMethodDispatch* dispatch,
                                 OSObject* target, void* reference) override;
    kern_return_t CopyClientMemoryForType(uint64_t type,
                                          uint64_t* options,
                                          IOMemoryDescriptor** memory) override;
};
```

**Matching (no physical hardware):**

```xml
<key>ISCSIBlockStorage</key>
<dict>
    <key>CFBundleIdentifierKernel</key>
    <string>com.apple.iokit.IOStorageFamily</string>
    <key>IOClass</key>
    <string>IOUserResources</string>
    <key>IOProviderClass</key>
    <string>IOUserResources</string>
    <key>IOMatchCategory</key>
    <string>com.peilinwu.iscsi-rs</string>
    <key>IOUserClass</key>
    <string>ISCSIBlockStorageDevice</string>
    <key>IOUserServerName</key>
    <string>com.peilinwu.iscsi-rs</string>
</dict>
```

**IOUserClient interface:**

| Selector | Direction | Purpose |
|----------|-----------|---------|
| `kSetGeometry` | Daemon → Dext | Set block size + block count, dext registers with kernel |
| `kReadComplete` | Daemon → Dext | Return read data + status for a pending read |
| `kWriteComplete` | Daemon → Dext | Return write status for a pending write |
| `kSyncComplete` | Daemon → Dext | Return sync status |
| `kNotifyIO` | Dext → Daemon | Notify daemon of pending I/O (via IODataQueueDispatchSource) |

**Notification mechanism (bidirectional):**
- **Dext → Daemon (I/O ready):** IODataQueueDispatchSource enqueues a notification when new I/O requests are in the ring. Daemon wakes on `DataAvailable` callback.
- **Daemon → Dext (completion ready):** Daemon calls `IOConnectCallScalarMethod` with `kReadComplete`/`kWriteComplete`/`kSyncComplete` selector and the request `id` as a scalar argument. This ExternalMethod call wakes the dext, which then dequeues from the completion ring and calls `CompleteIO`. The shared memory ring carries data; the ExternalMethod calls carry the wakeup signal.

**Dext behavior when daemon is absent:**
- If no daemon has connected (no IOUserClient open), `doAsyncReadWrite()` returns `kIOReturnNotReady`. The kernel/filesystem will retry.
- If the daemon disconnects mid-operation, pending I/Os time out after 30s and complete with `kIOReturnTimeout`. The dext tracks daemon liveness via a heartbeat field in the control region (daemon updates every 2s). If heartbeat is stale >10s, dext returns `kIOReturnNotReady` for new I/Os.
- On daemon reconnect, the dext re-accepts IOUserClient connections. Pending I/Os that timed out are not retried — the filesystem handles retries.

**Daemon crash and data integrity:**
- Write coalescing means the daemon may hold dirty data not yet flushed to iSCSI. If the daemon crashes after acknowledging a write to the dext but before `scsi_write()`, that data is lost. This is semantically equivalent to a disk with a volatile write cache — standard behavior for block devices.
- For safety-critical workloads, users can disable write coalescing (`write_coalesce_ms: 0`) so every write is flushed to iSCSI before completion is returned to the dext. This adds latency (~1 RTT per write) but guarantees persistence.
- `doSynchronize()` always flushes the dirty map and issues SYNCHRONIZE CACHE, ensuring fsync() semantics are honored regardless of coalescing settings.

### Shared Memory Layout

```
Region 0: Control (4KB, allocated by dext via IOBufferMemoryDescriptor::Create,
          mapped by daemon via IOConnectMapMemory64)
  geometry:  { block_size: u32, block_count: u64 }
  state:     { attached: bool, daemon_pid: u32, heartbeat_ts: u64 }
  stats:     { reads: u64, writes: u64, errs: u64 }
  read_only: bool

Region 1: I/O Request Ring (64KB)
  head: u32 (dext writes, daemon reads)  — atomic, release/acquire ordering
  tail: u32 (daemon writes, dext reads)  — atomic, release/acquire ordering
  entries[256]: {
    id: u32,
    op: u8,           // READ=0, WRITE=1, SYNC=2
    _pad: [u8; 3],    // align lba to 8 bytes
    lba: u64,
    block_count: u32,
    buffer_idx: u16,
    _pad2: [u8; 2],   // align entry to 24 bytes total
  }
  // Entry size: 24 bytes (explicit padding, no #pragma pack needed)
  // Both C++ and Rust use natural alignment (#[repr(C)])

Region 2: Completion Ring (16KB)
  head/tail (atomic, release/acquire) + entries[256]: {
    id: u32,
    status: u8,       // OK=0, ERROR=1
    _pad: [u8; 3],
    error_code: u32,
    _pad2: [u8; 4],   // align to 16 bytes
  }

Region 3: Data Buffer Pool (32MB)
  128 x 256KB slots
  slot[i] at offset i * 256KB
  Used for read data and write data

Memory ordering: All rings are SPSC (single-producer, single-consumer).
  - Producer writes entry, then updates head with store-release.
  - Consumer reads head with load-acquire, then reads entry.
  - C++ side: std::atomic<uint32_t> with memory_order_release/acquire.
  - Rust side: AtomicU32 with Ordering::Release/Acquire.
  - This is the standard lock-free SPSC ring buffer protocol.

Memory allocation: All regions are allocated by the dext using
  IOBufferMemoryDescriptor::Create(). The daemon maps them via
  IOConnectMapMemory64() with a type parameter selecting the region.

Buffer slot lifecycle:
  - Dext allocates a free slot from a 128-bit bitmap before enqueuing a request.
  - For WRITE: dext copies kernel IOMemoryDescriptor data into the slot, then enqueues.
  - For READ: daemon writes data into the slot, then enqueues completion.
  - Dext frees the slot after processing the completion (after copying read data
    back to kernel IOMemoryDescriptor, or after confirming write completion).
  - The bitmap is dext-local (not in shared memory) since only the dext allocs/frees.

Backpressure:
  - If no buffer slot is available (all 128 in use), doAsyncReadWrite() returns
    kIOReturnNoResources. The kernel block layer retries automatically.
  - If the I/O ring is full (256 entries), same behavior.
  - Configurable: buffer pool size can be tuned in the dext's Info.plist.
```

### Rust Daemon (`iscsid`)

**Module structure:**

```
src/
├── main.rs                    // Entry point, daemonization, signal handling
├── cli.rs                     // CLI tool (iscsi-rs binary, separate from daemon)
├── daemon/
│   ├── mod.rs                 // Daemon lifecycle, launchd integration
│   ├── ipc.rs                 // Unix socket server for CLI communication
│   └── session_manager.rs     // Manages multiple iSCSI sessions
├── dext/
│   ├── mod.rs                 // IOKit client: find dext, open IOUserClient
│   ├── shared_memory.rs       // mmap shared regions, ring buffer read/write
│   └── io_bridge.rs           // Dequeue I/O requests → dispatch to block layer
│                              // Enqueue completions ← receive from block layer
├── block/
│   ├── mod.rs                 // Block device layer (evolved from block_device.rs)
│   ├── cache.rs               // moka LRU cache + adaptive readahead (existing)
│   └── dirty_map.rs           // Write coalescing + flush (existing)
├── iscsi/
│   ├── pdu.rs                 // (existing, unchanged)
│   ├── transport.rs           // (existing, unchanged)
│   ├── login.rs               // (existing, unchanged)
│   ├── session.rs             // (existing, unchanged)
│   ├── command.rs             // (existing, unchanged)
│   ├── pipeline.rs            // (existing, + SYNCHRONIZE CACHE integration)
│   ├── recovery.rs            // (existing, unchanged)
│   ├── digest.rs              // (existing, unchanged)
│   └── config.rs              // (existing, extended for daemon config)
└── proto/
    └── ring.rs                // Ring buffer data structures (#[repr(C)])
```

**Code reuse from iscsi-fuse:**

| Module | Change | Reason |
|--------|--------|--------|
| `fuse_fs.rs` | Deleted | Replaced by dext |
| `auto_format.rs` | Deleted | Real `/dev/diskN` works with APFS natively |
| `block_device.rs` | Refactored → `block/mod.rs` | Same logic, new I/O source |
| `cache.rs` | Moved → `block/cache.rs` | Unchanged |
| `main.rs` | Rewritten | Daemon mode, launchd, no FUSE mount |
| `iscsi/*` | Unchanged | iSCSI stack fully reused |
| `pipeline.rs` | Significant add | New `build_synchronize_cache()` CDB builder in `command.rs`, new `synchronize_cache()` method on `Pipeline`, integration into dirty map flush path. Required for write persistence — not optional. |

### CLI Tool (`iscsi-rs`)

| Command | Purpose |
|---------|---------|
| `iscsi-rs discover <portal>` | Discover targets via SendTargets |
| `iscsi-rs login <target>` | Connect to target, attach block device |
| `iscsi-rs logout <target>` | Disconnect, detach block device |
| `iscsi-rs list` | Show active sessions |
| `iscsi-rs status` | Daemon + dext health |
| `iscsi-rs activate` | Trigger dext activation (first install) |

Communicates with daemon via Unix domain socket at `/var/run/iscsid.sock`.

### Multi-Target Architecture

Each iSCSI target/LUN gets its own independent block device:
- The dext creates one `ISCSIBlockStorageDevice` instance per target (each gets its own `/dev/diskN`)
- Each instance has its own set of shared memory regions (control, I/O ring, completion ring, data pool)
- The daemon's `SessionManager` maps each target to its own iSCSI session + block layer + dext instance
- The IOUserClient connection carries a target identifier to route to the correct device instance

### Device Registration Timing

The dext does NOT register the block device at `Start()` time. Registration is deferred:
1. Dext starts and waits for daemon connection (IOUserClient open)
2. Daemon connects, sends `kSetGeometry` with block size and count
3. Dext stores geometry, then calls `registerService()` to register the block device
4. Kernel calls `ReportBlockSize()` / `ReportMaxBlockCount()` — dext returns stored values
5. Kernel creates `/dev/diskN`

This avoids exposing a device with unknown geometry. On daemon disconnect, the dext can optionally call `terminate()` to remove the `/dev/diskN`.

### Read-Only Support

The daemon sends a `read_only` flag in the control region. When set:
- `ReportMediumWritable()` returns `false`
- `doAsyncReadWrite()` rejects writes with `kIOReturnNotWritable`
- The kernel mounts the filesystem read-only

### Configuration

Path: `/etc/iscsi-rs/config.json`

```json
{
  "targets": [
    {
      "iqn": "iqn.2004-04.com.qnap:ts-873a:iscsi.lun1",
      "portal": "192.168.2.57:3260",
      "lun": 0,
      "auth": {
        "method": "none"
      },
      "cache": {
        "size_mb": 1024,
        "readahead_max_kb": 512,
        "write_coalesce_ms": 5,
        "write_coalesce_max_kb": 1024
      }
    }
  ],
  "daemon": {
    "log_level": "info",
    "socket_path": "/var/run/iscsid.sock",
    "pid_file": "/var/run/iscsid.pid"
  }
}
```

## Startup Sequence

```
1. launchd starts iscsid daemon
2. iscsid loads config from /etc/iscsi-rs/config.json
3. iscsid calls IOServiceGetMatchingService() to find dext
   └─ If dext not found (not approved yet):
      └─ iscsid logs error, starts Unix socket server, waits
      └─ CLI: `iscsi-rs status` shows "dext not loaded — approve in System Settings"
4. iscsid calls IOServiceOpen() to create IOUserClient connection
5. iscsid calls IOConnectMapMemory64() for each shared memory region
6. iscsid starts Unix socket server at /var/run/iscsid.sock
7. iscsid waits for CLI commands...

   User runs: iscsi-rs login <target>
   8.  iscsid connects TCP to iSCSI target
   9.  iscsid performs iSCSI login + parameter negotiation
   10. iscsid issues READ CAPACITY to get geometry
   11. iscsid writes geometry to shared memory control region
   12. iscsid calls ExternalMethod(kSetGeometry) on dext
   13. Dext stores geometry, calls registerService()
   14. Kernel creates /dev/diskN
   15. macOS auto-mounts if filesystem is recognized
   16. iscsid starts I/O processing loop (polling I/O ring)
```

**First-run experience:** If the dext has never been approved, macOS shows a notification: "System Extension Blocked." The user must open System Settings > Privacy & Security and click "Allow." The CLI tool (`iscsi-rs activate`) triggers this flow and provides guidance if the dext is not yet approved.

## Data Flow

### Read Path

1. App reads file on `/Volumes/MyLUN/`
2. APFS → kernel block layer → `doAsyncReadWrite(READ, lba, nblks)`
3. Dext enqueues request to I/O ring, signals daemon
4. Daemon checks block cache → hit: return cached data; miss: `scsi_read()` over iSCSI
5. Daemon writes data into shared buffer slot, enqueues completion
6. Dext copies buffer to kernel IOMemoryDescriptor, calls `CompleteIO(kIOReturnSuccess)`

### Write Path

1. App writes file
2. APFS → kernel → `doAsyncReadWrite(WRITE, lba, nblks)`
3. Dext copies kernel data to shared buffer, enqueues request
4. Daemon dequeues, merges into dirty map (write coalescing)
5. On flush timer (5ms) or threshold (1MB): `scsi_write()` over iSCSI
6. Daemon enqueues completion → dext calls `CompleteIO`

### Sync Path

1. `fsync()` or APFS journal flush → `doSynchronize()`
2. Dext enqueues SYNC request
3. Daemon flushes all dirty map entries, sends SYNCHRONIZE CACHE SCSI command
4. Daemon enqueues completion → dext calls `CompleteIO`

### Key Fix

```
Old: App → APFS → DiskImages buffer (LOST) → FUSE → daemon → iSCSI
New: App → APFS → kernel block I/O → dext → shared mem → daemon → iSCSI
```

No DiskImages layer. Every write reaches the daemon. Every sync reaches the target.

## Distribution

### Installation Structure

```
/Applications/iSCSI-RS.app/                          ← minimal app bundle
  Contents/
    Library/SystemExtensions/
      com.peilinwu.iscsi-rs.dext/                    ← DriverKit extension
    MacOS/iscsi-rs-activator                         ← triggers dext activation
    Info.plist

/usr/local/bin/iscsid                                ← Rust daemon
/usr/local/bin/iscsi-rs                              ← CLI tool
/Library/LaunchDaemons/com.peilinwu.iscsid.plist     ← launchd plist
/etc/iscsi-rs/config.json                            ← configuration
```

### Homebrew Cask

```
brew tap dickwu/iscsi-rs
brew install --cask iscsi-rs
```

Cask formula installs `.pkg`, postflight triggers dext activation. User approves in System Settings > Privacy & Security.

### GitHub Actions CI/CD

**On every push / PR (`.github/workflows/ci.yml`):**
- `cargo check`, `cargo fmt`, `cargo clippy`
- `cargo test` (unit + integration)
- `xcodebuild` dext (debug config)

**On tag `v*` (`.github/workflows/release.yml`):**
- Build Rust binaries (universal: arm64 + x86_64 via lipo)
- Build dext via xcodebuild (release, code-signed)
- Build .app bundle (embed dext + activator)
- Build .pkg installer (productbuild)
- Code sign + notarize .pkg (notarytool)
- Create GitHub Release with assets:
  - `iscsi-rs-v{version}-macos-universal.pkg`
  - `iscsi-rs-v{version}-macos-universal.tar.gz`
- Compute SHA256 for Homebrew

**Required GitHub secrets:**
- `APPLE_DEVELOPER_ID_APPLICATION` — code signing cert
- `APPLE_DEVELOPER_ID_INSTALLER` — .pkg signing cert
- `APPLE_ID` / `APPLE_TEAM_ID` / `APPLE_APP_PASSWORD` — notarization
- `DEXT_PROVISIONING_PROFILE` — DriverKit provisioning

### Updated `/publish` Skill

The publish skill changes from the iscsi-fuse flow:

| Current | New |
|---------|-----|
| Repo: `dickwu/iscsi-fuse` | Repo: `dickwu/iscsi-rs` |
| Tap: `dickwu/homebrew-iscsi-fuse` | Tap: `dickwu/homebrew-iscsi-rs` |
| Formula (regular brew) | Cask (`brew --cask`) |
| `cargo check` only | `cargo check` + `xcodebuild` |
| Pre-built binary tarball | `.pkg` installer + tarball |
| `brew install iscsi-fuse` | `brew install --cask iscsi-rs` |
| No signing/notarization | Code sign + notarize via CI |

New publish flow:
1. Bump version in `Cargo.toml` + `Info.plist` (dext)
2. `cargo check` + `cargo fmt` + `cargo clippy`
3. `xcodebuild` dext in release mode
4. Commit, tag, push
5. Wait for GitHub Actions release workflow
6. Download release asset, compute SHA256
7. Update Homebrew cask formula
8. Commit + push tap repo
9. `brew upgrade --cask iscsi-rs`
10. Verify: `iscsi-rs status`, `iscsid --version`

## Implementation Phases

### Phase 1: Fix Write Persistence (current repo, iscsi-fuse)
1. Write integration test: write → SYNCHRONIZE CACHE → disconnect → reconnect → read → assert
2. Add SYNCHRONIZE CACHE call after dirty map flush in pipeline
3. Debug Data-Out PDU construction if test still fails
4. Validate with QNAP target
5. **Gate:** integration test passes reliably against QNAP target. For CI, use an open-source iSCSI target (tgt or LIO in a Linux VM/container) to avoid hardware dependency.

### Phase 2: Scaffold iscsi-rs Repo
1. **Apply for DriverKit entitlements immediately** — `com.apple.developer.driverkit.family.block-storage-device` requires Apple approval (days to weeks). This is a blocking external dependency; do not defer to Phase 7.
2. Rename repo / create new repo `iscsi-rs`
3. Set up Xcode project for dext
4. Set up Cargo workspace: `iscsid` (daemon) + `iscsi-rs` (CLI) binaries
5. Port existing iSCSI modules into new structure
6. Remove FUSE code (`fuse_fs.rs`, `auto_format.rs`, `fuser` dependency)
7. **Gate:** project builds, iSCSI stack unit tests pass, DriverKit entitlement approved

### Phase 3: DriverKit Dext
1. Implement ISCSIBlockStorageDevice (IOUserBlockStorageDevice subclass)
2. Implement ISCSIUserClient (ExternalMethod dispatch, CopyClientMemoryForType)
3. Shared memory allocation (control, I/O ring, completion ring, data pool)
4. IODataQueueDispatchSource for notifications
5. Info.plist with IOUserResources matching
6. Entitlements file:
   ```xml
   <key>com.apple.developer.driverkit</key> <true/>
   <key>com.apple.developer.driverkit.family.block-storage-device</key> <true/>
   <key>com.apple.developer.driverkit.transport.userclient</key> <true/>
   ```
7. **Gate:** dext loads, creates `/dev/diskN` with hardcoded geometry, AND a test client can open IOUserClient and map shared memory regions

### Phase 4: Rust-Dext Bridge
1. IOKit client in Rust (`iokit-sys` FFI): find dext, open IOUserClient
2. `IOConnectMapMemory64` to map shared regions
3. Ring buffer implementation (`#[repr(C)]` matching C++ layout)
4. I/O bridge: dequeue requests → block layer → enqueue completions
5. Geometry handshake: daemon writes geometry → dext registers device
6. **Gate:** `dd if=/dev/diskN` reads data from iSCSI target through full stack

### Phase 5: Daemon & CLI
1. Daemonize with launchd plist
2. Unix socket IPC for CLI-daemon communication
3. CLI commands: login, logout, list, status, activate
4. Session manager for multi-target
5. Config loading from `/etc/iscsi-rs/config.json`
6. Signal handling (SIGTERM graceful shutdown)
7. **Gate:** `iscsi-rs login` connects, `/dev/diskN` appears, `diskutil info` works

### Phase 6: Formatting & Filesystem Validation
1. Test `newfs_apfs /dev/diskN` — should work natively
2. Test `newfs_hfs /dev/diskN` — should also work
3. Test write persistence: format → write files → unmount → remount → files intact
4. Test `diskutil mount /dev/diskN` auto-mount
5. **Gate:** APFS format + file persistence works end-to-end

### Phase 7: Performance Testing
1. Benchmark sequential read/write throughput (`dd` with various block sizes)
2. Benchmark random IOPS (`fio` or custom test)
3. Measure IPC latency overhead (dext-daemon round-trip)
4. Compare against FUSE path baseline (from iscsi-fuse) for regression check
5. Profile under load: buffer slot utilization, ring occupancy, cache hit rate
6. **Gate:** throughput within 10% of theoretical iSCSI link speed for sequential I/O

### Phase 8: Distribution & CI/CD
1. GitHub Actions CI workflow (check, test, xcodebuild on push/PR)
2. GitHub Actions release workflow (build, sign, notarize, release on tag)
3. Homebrew cask formula (`iscsi-rs.rb`)
4. `.pkg` installer as alternative
5. App bundle with embedded dext
6. Code signing + notarization pipeline
7. Update `/publish` skill for new project structure
8. README, installation guide, troubleshooting
9. **Gate:** `brew install --cask iscsi-rs` works on a clean Mac

## Research References

- [BlockStorageDeviceDriverKit](https://developer.apple.com/documentation/blockstoragedevicedriverkit)
- [IOUserBlockStorageDevice](https://developer.apple.com/documentation/blockstoragedevicedriverkit/iouserblockstoragedevice)
- [DriverKit Entitlements](https://developer.apple.com/documentation/driverkit/requesting-entitlements-for-driverkit-development)
- [Apple: Communicating Between a DriverKit Extension and a Client App](https://developer.apple.com/documentation/driverkit/communicating-between-a-driverkit-extension-and-a-client-app)
- [Karabiner-DriverKit-VirtualHIDDevice](https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice) — validates virtual device + IOUserClient pattern
- [MacVFN](https://github.com/SamsungDS/MacVFN) — DriverKit block storage proof-of-concept
- [iSCSI-osx/iSCSIInitiator](https://github.com/iscsi-osx/iSCSIInitiator) — confirms DriverKit socket limitation
- [WWDC 2020: Modernize PCI and SCSI Drivers with DriverKit](https://developer.apple.com/videos/play/wwdc2020/10210/)
- [IODataQueueDispatchSource](https://developer.apple.com/documentation/driverkit/iodataqueuedispatchsource)
