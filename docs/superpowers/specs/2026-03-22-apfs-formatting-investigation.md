# APFS Formatting Investigation Report

**Date:** 2026-03-22
**Target:** QNAP TS-873A (192.168.2.57:3260), 2 TiB iSCSI LUN, 512-byte sectors

## Problem

After mounting an iSCSI target via iscsi-fuse, formatting with APFS fails:

```
obj_checksum_verify_phys:52: failed: cksum 0x0000000000000000, oid 0x0, type 0x0/0x0, size 4096
nx_corruption_detected_int:39: Container corruption detected by obj_checksum_verify_phys:54!
spaceman_allocated:1280: rdisk4 failed to read cib 0: 92
nx_format:173: spaceman initialization failed: 92 - Illegal byte sequence
newfs_apfs: unable to format /dev/disk4: Illegal byte sequence
```

## I/O Path

```
newfs_apfs → /dev/disk4 → DiskImages kernel driver → FUSE kernel → FUSE userspace → iSCSI target
```

The FUSE filesystem exposes a single file (`disk.img`) at the mount point. `hdiutil attach -imagekey diskimage-class=CRawDiskImage` creates `/dev/diskN` backed by that file.

## Root Causes Found

### 1. DiskImages driver does not flush writes to FUSE backing file

The macOS DiskImages driver for `CRawDiskImage` maintains its own internal buffer cache. Writes through `/dev/diskN` go into this buffer but are **never flushed to the FUSE backing file**. This was proven by:

- Writing data through `/dev/disk4` (dd, newfs_hfs, newfs_apfs)
- Reading back through `/dev/disk4` — succeeds (served from DiskImages buffer)
- Detaching and re-attaching — data is gone (FUSE file still has zeros)
- Reading the FUSE file directly — zeros (write never reached FUSE)

This means **no filesystem format can persist** through the `hdiutil attach` path, regardless of filesystem type.

### 2. APFS uses kernel-internal I/O paths

I/O tracing of `newfs_apfs` showed:

- 64 FUSE writes, **all zeros** (the disk-zeroing phase)
- Non-zero writes at high offsets (~115 MB) for APFS structures
- **Read returns different data than what was written** — proving the DiskImages driver's internal cache diverges from the FUSE backing store
- APFS's kernel implementation has deep integration with the DiskImages driver that doesn't respect FUSE semantics

### 3. FOPEN_DIRECT_IO breaks DiskImages entirely

Setting `FOPEN_DIRECT_IO` in the FUSE `open()` handler (to bypass the kernel page cache) causes the DiskImages driver to return `EINVAL` on **all** reads and writes through both `/dev/disk4` and `/dev/rdisk4`. The DiskImages layer fundamentally requires kernel page cache cooperation.

### 4. Write persistence bug (separate issue)

Even direct writes to the FUSE file (`dd of=/Volumes/iscsi/disk.img`) that successfully reach FUSE userspace and produce `SCSI WRITE` commands with `scsi_status=Good` from the target **do not persist across FUSE restarts**. After unmounting and remounting, the target returns zeros for previously-written LBAs. This is a separate bug in the SCSI WRITE path that needs investigation.

## What Works

| Operation | Result |
|-----------|--------|
| dd write + read (same session, via /dev/disk) | Works (DiskImages buffer cache) |
| dd write + read (same session, via FUSE file) | Works (kernel page cache) |
| `newfs_hfs` format | Reports success (DiskImages buffer) |
| `newfs_apfs` format | Fails with EILSEQ |
| Write persistence across restarts | **Fails** — data doesn't survive unmount/remount |
| `FOPEN_DIRECT_IO` | Breaks DiskImages entirely (EINVAL) |

## Attempted Fixes

### Sync-write mode (implemented, v0.4.0)

Added `--sync-writes` flag and `sync_writes` field to `BlockDeviceWorker`. When enabled, every write is flushed to iSCSI immediately instead of waiting for the coalesce timer. This eliminates dirty map coherence issues but doesn't fix the DiskImages problem.

### Auto-format (implemented, v0.4.0)

Added `--auto-format` flag that spawns a background thread to:
1. Wait for FUSE mount
2. `hdiutil attach` the disk image
3. Format with `newfs_hfs` (HFS+ Journaled)
4. `diskutil mountDisk` the volume
5. Switch back to async writes

The format succeeds within the DiskImages buffer, but data doesn't persist to iSCSI due to the write persistence bug.

## Architecture Limitation

The fundamental problem is that macOS's DiskImages framework treats the FUSE file as a **dumb backing store** and maintains its own in-memory state that never synchronizes back. This is a kernel-level architectural limitation — not something fixable in userspace FUSE code.

```
                    ┌─────────────────────────┐
                    │    DiskImages Driver     │
                    │  ┌───────────────────┐  │
newfs_apfs ────────►│  │  Internal Buffer  │  │
/dev/disk4          │  │  Cache (R/W)      │  │
                    │  └───────┬───────────┘  │
                    │          │ rarely/never  │
                    │          ▼ flushes       │
                    │  ┌───────────────────┐  │
                    │  │  FUSE file write  │──┼──► FUSE userspace ──► iSCSI
                    │  │  (backing store)  │  │
                    │  └───────────────────┘  │
                    └─────────────────────────┘
```

## Recommended Next Steps

1. **Fix write persistence bug** — SCSI WRITE returns Good but data doesn't survive reconnect. Debug the SCSI WRITE PDU construction and verify data actually reaches the target's persistent storage. May need `SYNCHRONIZE CACHE` SCSI command after writes.

2. **Consider DriverKit** — macOS DriverKit System Extensions can expose proper `/dev/disk` block devices without the DiskImages intermediary. This eliminates the FUSE+DiskImages layering entirely. Major engineering effort but the correct long-term solution.

3. **Pre-format via QNAP** — As a workaround, format the iSCSI LUN using the QNAP NAS web interface (Storage & Snapshots → iSCSI target), then mount the already-formatted volume with iscsi-fuse.
