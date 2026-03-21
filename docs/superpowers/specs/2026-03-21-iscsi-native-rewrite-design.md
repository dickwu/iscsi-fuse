# iSCSI Native Protocol Rewrite — Design Spec

**Date:** 2026-03-21
**Status:** Approved
**Goal:** Replace `iscsi-client-rs` dependency with a native iSCSI initiator implementation following RFC 7143, optimized for 10Gbps network throughput.

## Summary of Decisions

| Decision | Choice |
|----------|--------|
| Authentication | AuthMethod=None only |
| Error Recovery | ERL=0 + automatic session recovery + NOP keepalive |
| Command Depth | 128 outstanding commands (aggressive pipelining) |
| Connections | Single TCP connection per session |
| CRC32C Digests | Always negotiate CRC32C for header + data |
| FUSE Threading | Multi-threaded FUSE + async backend via channels |
| Cache | moka + Bytes, 64KB chunks, adaptive readahead, write coalescing |
| Config Format | TOML (simplified, 10G-optimized defaults) |
| Delivery | Full parity + performance, split into small tasks |

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│  FUSE (fuser, multi-threaded, N = num_cpus)                  │
│  fuse_fs.rs — read/write/readdir/statfs callbacks            │
└────────────────────────┬─────────────────────────────────────┘
                         │ crossbeam channel (request/response)
┌────────────────────────▼─────────────────────────────────────┐
│  Block Device Layer                                          │
│  block_device.rs — byte-offset → LBA translation,            │
│                    write coalescing (5ms / 1MB flush),       │
│                    read-modify-write for unaligned writes     │
└──────┬──────────────────────────────────┬────────────────────┘
       │ cache check                      │ SCSI command
┌──────▼──────────┐              ┌────────▼────────────────────┐
│  Cache Layer    │              │  Command Pipeline            │
│  cache.rs       │              │  pipeline.rs — 128-deep      │
│  moka + Bytes   │              │  command window, ITT pool,   │
│  adaptive       │              │  completion tracking,         │
│  readahead      │              │  backpressure                │
└─────────────────┘              └────────┬────────────────────┘
                                          │ submit PDU / recv PDU
                                 ┌────────▼────────────────────┐
                                 │  Session                     │
                                 │  session.rs — CmdSN/StatSN   │
                                 │  windowing, sequence numbers │
                                 └────────┬────────────────────┘
                                          │
                                 ┌────────▼────────────────────┐
                                 │  Transport                   │
                                 │  transport.rs — TCP stream,   │
                                 │  PDU framing, socket tuning  │
                                 │  (4MB buffers, TCP_NODELAY)  │
                                 └────────┬────────────────────┘
                                          │
                                 ┌────────▼────────────────────┐
                                 │  PDU Layer                   │
                                 │  pdu.rs — serialize/          │
                                 │  deserialize BHS + data      │
                                 │  + digest (zero-copy Bytes)  │
                                 └─────────────────────────────┘
```

**Data flow — SCSI READ:**
1. FUSE thread receives `read(offset, size)` from kernel
2. FUSE sends request via channel to block device worker task
3. Block device translates to LBA range, checks moka cache
4. Cache miss → pipeline submits SCSI READ command (assigns ITT, checks CmdSN window)
5. Session stamps CmdSN/ExpStatSN, passes to transport
6. Transport serializes PDU via `pdu.rs`, writes to TCP with CRC32C header/data digest
7. Transport reads response PDUs (Data-In), verifies digests
8. Pipeline matches response to ITT, completes the oneshot future
9. Block device receives data as `Bytes`, inserts into cache, returns to FUSE
10. Readahead: if sequential access detected, cache spawns async prefetch for next window

**Data flow — SCSI WRITE:**
1. FUSE write → block device coalescing dirty map
2. On flush trigger (5ms timer / 1MB threshold / fsync): merge adjacent LBA ranges
3. Pipeline submits SCSI WRITE command with ImmediateData (first burst piggybacked)
4. Target sends R2T → session receiver sends Data-Out PDUs
5. Target sends SCSI Response → pipeline completes → cache invalidated → FUSE reply

## Module Design

### File Structure

```
src/
  iscsi/
    mod.rs          -- public API (re-exports)
    pdu.rs          -- PDU types, BHS serialization/deserialization
    transport.rs    -- TCP connection, socket tuning, PDU framing
    login.rs        -- login state machine, parameter negotiation
    session.rs      -- CmdSN/StatSN windowing, ITT allocation, response routing
    command.rs      -- SCSI CDB builders + response parsers
    pipeline.rs     -- 128-deep command window, R2T write flow
    digest.rs       -- CRC32C hardware-accelerated (Apple Silicon)
    recovery.rs     -- ERL=0 session recovery, NOP keepalive, I/O queuing
    config.rs       -- TOML config with 10G defaults
  cache.rs          -- moka concurrent cache, adaptive readahead
  block_device.rs   -- channel-based async dispatch, write coalescing
  fuse_fs.rs        -- multi-threaded FUSE filesystem
  main.rs           -- startup, wiring, shutdown
```

### 1. PDU Layer (`iscsi/pdu.rs`)

All iSCSI communication is PDU-based. This module defines the wire format.

**Types:**
- `Opcode` — enum of all initiator (0x00-0x10) and target (0x20-0x3f) opcodes
- `Bhs` — 48-byte Basic Header Segment, parsed zero-copy from `[u8; 48]`
- `Pdu` — complete PDU: `Bhs` + optional `Bytes` AHS + optional `Bytes` data segment

**Serialization:**
- `Pdu::serialize_bhs() -> [u8; 48]` — stack-allocated, no heap for the hot path
- `Pdu::serialize(&self, buf: &mut BytesMut, digests: &DigestConfig)` — full PDU with digests

**Deserialization:**
- `Bhs::parse(buf: &[u8; 48]) -> Result<Self>` — no allocation
- Data segments returned as `Bytes` — zero-copy from TCP read buffer into cache

**Opcode-specific accessors** on `Bhs`:
- SCSI Response: `scsi_status()`, `stat_sn()`, `exp_cmd_sn()`, `max_cmd_sn()`
- Data-In: `data_sn()`, `buffer_offset()`, `status_flag()`
- R2T: `ttt()`, `r2t_sn()`, `r2t_buffer_offset()`, `r2t_desired_length()`

**Builders** on `Bhs`:
- `build_scsi_command(lun, itt, cmd_sn, exp_stat_sn, cdb, edtl, read, write)`
- `build_data_out(lun, itt, ttt, exp_stat_sn, data_sn, buffer_offset)`
- `build_login_request(isid, tsih, cid, itt, cmd_sn, exp_stat_sn, csg, nsg, transit)`
- `build_nop_out(itt, ttt, cmd_sn, exp_stat_sn)`
- `build_logout_request(itt, cmd_sn, exp_stat_sn, cid)`
- `build_snack_request(itt, ttt, exp_stat_sn, beg_run, run_length)`

**Design decisions:**
- BHS always stack-allocated `[u8; 48]`
- Data segments use `Bytes` for zero-copy flow from TCP → cache
- All multi-byte fields big-endian on wire, native in memory
- Padding to 4-byte boundary handled in serialize/deserialize

### 2. Transport Layer (`iscsi/transport.rs`)

Owns the TCP connection. Only module that touches the socket.

**Type:** `Transport` — holds `BufReader<OwnedReadHalf>` + `BufWriter<OwnedWriteHalf>` (tokio split) + `DigestConfig` + pre-allocated `[u8; 48]` BHS read buffer.

**Connection setup (10G tuning):**
- `SO_SNDBUF = 4MB`, `SO_RCVBUF = 4MB` via `socket2::SockRef`
- `TCP_NODELAY = true`
- 1MB userspace read/write buffers
- TCP keepalive: `TCP_KEEPIDLE=30s`, `TCP_KEEPINTVL=5s`, `TCP_KEEPCNT=3`

**Core methods:**
- `send_pdu(&mut self, pdu: &Pdu)` — vectored write (`write_vectored`) of BHS + AHS + header digest + data + data digest. Single syscall, no memcpy.
- `recv_pdu(&mut self) -> Result<Pdu>` — read exactly 48 bytes for BHS, verify header digest, read data segment into `BytesMut`, verify data digest, `.freeze()` to `Bytes`.
- `enable_digests(&mut self, config: DigestConfig)` — called after login negotiation.

**Design decisions:**
- `TcpStream` split into owned halves — concurrent send (from command submission) and recv (from receiver task)
- Vectored writes avoid memcpy of BHS + data into contiguous buffer
- Digests start disabled (login phase), enabled after negotiation
- Data arrives as `Bytes` — flows to cache without copying

### 3. Login Phase (`iscsi/login.rs`)

Two-phase login: Security Negotiation → Operational Negotiation → Full Feature Phase.

**Types:**
- `LoginManager` — holds ISID, initiator name, target name
- `LoginResult` — tsih + `NegotiatedParams`
- `NegotiatedParams` — all negotiated operational parameters

**Flow:**
1. Security phase (CSG=0, NSG=1, T=1): single exchange for AuthMethod=None. Sends InitiatorName, TargetName, SessionType=Normal, AuthMethod=None. Receives TSIH.
2. Operational phase (CSG=1, NSG=3, T=1): propose 10G-optimized parameters, parse target response.

**10G proposals:**
- `MaxRecvDataSegmentLength=1048576` (1MB)
- `MaxBurstLength=1048576` (1MB)
- `FirstBurstLength=262144` (256KB)
- `InitialR2T=No` (allow unsolicited data)
- `ImmediateData=Yes` (piggyback in command PDU)
- `MaxOutstandingR2T=8`
- `HeaderDigest=CRC32C`
- `DataDigest=CRC32C`
- `ErrorRecoveryLevel=0`

**Negotiation rules:** min for numerical, AND for boolean, list-select for digests. Unknown keys ignored per RFC 7143.

### 4. Session Management (`iscsi/session.rs`)

Central coordinator — sequence numbers, ITT allocation, transport ownership.

**Types:**
- `Session` — holds `Mutex<TransportWriter>` (for multi-threaded command submission), `NegotiatedParams`, `SessionState`
- `SessionState` — atomics: `cmd_sn`, `exp_stat_sn`, `exp_cmd_sn`, `max_cmd_sn` (lock-free hot path)
- `IttPool` — `AtomicU128` bitmap for 128 ITT slots + `oneshot::Sender` completion channels

**ITT allocation (lock-free):**
- `alloc() -> Option<(u32, oneshot::Receiver)>` — CAS on `AtomicU128`, find first free bit
- `free(itt)` — atomic bit clear
- `complete(itt, response)` — send on oneshot, free slot

**Command submission:**
1. Wait for CmdSN window space (serial number arithmetic, `cmd_sn <= max_cmd_sn`)
2. Allocate ITT (wait if all 128 in use — backpressure)
3. Stamp CmdSN (atomic fetch_add) and ExpStatSN (atomic load)
4. Build PDU, send via transport writer

**Response receiver (dedicated tokio task):**
- Reads PDUs in a loop via `transport.recv_pdu()`
- Updates ExpStatSN, ExpCmdSN, MaxCmdSN from every target PDU
- Routes by opcode:
  - `ScsiResponse` → complete ITT with status + sense
  - `ScsiDataIn` → accumulate data by buffer offset; if S bit set, complete ITT
  - `R2T` → send Data-Out PDUs using registered write data
  - `NopIn` → respond with NOP-Out (keepalive)
  - `AsyncMessage` / `Reject` → log and handle

**Design decisions:**
- Atomics for sequence numbers — no mutex on hot path
- `AtomicU128` for ITT bitmap — single CAS for alloc/free
- `oneshot::channel` per command — zero-cost completion
- Transport split: writer behind Mutex, reader in dedicated task

### 5. SCSI Commands (`iscsi/command.rs`)

Pure functions — CDB builders and response parsers. No I/O, no state.

**CDB builders** (all return `[u8; 16]`):
- `build_test_unit_ready()`
- `build_inquiry(alloc_len)`
- `build_read_capacity10()`
- `build_read_capacity16(alloc_len)`
- `build_read10(lba, block_count)`, `build_read16(lba, block_count)`
- `build_write10(lba, block_count)`, `build_write16(lba, block_count)`
- `build_read(lba, block_count)` — auto-selects 10 vs 16
- `build_write(lba, block_count)` — auto-selects 10 vs 16

**Response parsers:**
- `parse_read_capacity10(data) -> (max_lba: u32, block_len: u32)`
- `parse_read_capacity16(data) -> (max_lba: u64, block_len: u32)`
- `parse_sense_data(data) -> SenseData` — handles 2-byte iSCSI length prefix + fixed format (0x70/0x71)

**Types:**
- `ScsiStatus` — Good, CheckCondition, Busy, TaskSetFull, etc.
- `SenseData` — sense_key, asc, ascq, information
- `SenseKey` — NoSense, NotReady, MediumError, UnitAttention, etc.
- `is_unit_attention(sense)`, `is_retryable(status, sense)`

### 6. Command Pipeline (`iscsi/pipeline.rs`)

Throughput engine — manages 128 concurrent commands.

**Read path:**
- `scsi_read(lba, block_count)` — splits into chunks sized by `max_burst_length / block_size`, submits ALL chunks concurrently. ITT pool provides natural backpressure at 128. Collects results in LBA order, returns `Bytes`.
- UNIT ATTENTION retry on single-read level.
- 30s timeout per command.

**Write path:**
- `scsi_write(lba, data)` — splits into chunks, pipelines submission.
- Each chunk: submit SCSI Command with ImmediateData (first burst piggybacked up to FirstBurstLength).
- R2T handling: session receiver reads R2T, sends Data-Out PDUs at requested buffer offset using `data.slice()` (zero-copy).
- 300s timeout (thin-provisioned LUN allocation).

**Capacity query:**
- `read_capacity()` — tries RC(10), falls back to RC(16) for >2TB, retries on UNIT ATTENTION (up to 3 times).

### 7. CRC32C Digest (`iscsi/digest.rs`)

Thin wrapper around `crc32c` crate. Hardware-accelerated on Apple Silicon (ARMv8 CRC instructions).

**Functions:**
- `header_digest(bhs, ahs) -> [u8; 4]`
- `verify_header_digest(bhs, ahs, received) -> Result<(), DigestError>`
- `data_digest(data) -> [u8; 4]`
- `verify_data_digest(data, received) -> Result<(), DigestError>`

**Performance:** ~0.3 cycles/byte on M-series. 1MB data ≈ 100μs. <1% CPU at 10GbE line rate.

### 8. Error Recovery (`iscsi/recovery.rs`)

ERL=0 + automatic session recovery. Based on research: no production initiator implements ERL=1 or SNACK. Every major initiator (open-iscsi, FreeBSD, Windows, ESXi) uses ERL=0 with session recovery.

**RecoveryManager:**
- Owns session, login manager, target address, pending I/O queue.
- `RecoveryConfig`: noop_interval (5s), noop_timeout (5s), replacement_timeout (30s), max_login_retries (6), login_retry_delay (5s).

**NOP-Out/NOP-In keepalive (background task):**
- Send NOP-Out after 5s of idle time (reset on any received PDU).
- If no NOP-In within 5s → connection dead → trigger recovery.

**Session recovery flow:**
1. Drain outstanding commands into pending queue
2. Reconnect: new TCP connection + fresh login (TSIH=0, new session)
3. TEST UNIT READY to clear UNIT ATTENTION
4. Retry queued commands (reads always safe; writes safe because same data to same LBA)
5. Expire commands that exceeded replacement_timeout → fail with EIO

**CRC32C digest errors:**
- Header mismatch → connection corrupt → trigger session recovery
- Data mismatch → trigger session recovery (no SNACK)

**Timeout values:**

| Parameter | Value | Source |
|-----------|-------|--------|
| NOP-Out interval | 5s | open-iscsi, FreeBSD standard |
| NOP-Out timeout | 5s | 10s total detection time |
| Login timeout | 15-30s | Balance speed vs slow networks |
| Replacement timeout | 30s | Queue I/O during recovery |
| TCP keepidle | 30s | Catch half-open connections |
| TCP keepintvl | 5s | Probe interval |
| TCP keepcnt | 3 | Fail after 3 unanswered |
| SCSI read timeout | 30s | Current value, proven |
| SCSI write timeout | 300s | Thin-provisioned LUN allocation |

### 9. Config (`iscsi/config.rs`)

TOML config at `~/.iscsi-fuse.toml`. Only `target` and `address` required.

**Minimal config:**
```toml
target = "iqn.2004-04.com.example:target"
address = "192.168.1.100:3260"
```

**Full config with all optional fields:**
```toml
target = "iqn.2004-04.com.example:target"
address = "192.168.1.100:3260"
initiator = "iqn.2024-01.com.iscsi-fuse:initiator"
lun = 0

[tuning]
max_recv_data_segment_length = 1048576
max_burst_length = 1048576
first_burst_length = 262144
max_outstanding_r2t = 8
immediate_data = true
initial_r2t = false
header_digest = true
data_digest = true

[recovery]
noop_interval_secs = 5
noop_timeout_secs = 5
replacement_timeout_secs = 30
max_login_retries = 6
login_retry_delay_secs = 5

[cache]
size_mb = 128
readahead_max_kb = 8192
write_coalesce_ms = 5
write_coalesce_max_kb = 1024
```

**CLI args** override config values: `--lun`, `--cache-size-mb`, `--read-only`, `--mount-point`, `--volume-name`, `--device-filename`, `--config`.

### 10. Cache Layer (`cache.rs`)

Concurrent cache with moka + Bytes, adaptive readahead.

**Design:**
- `moka::future::Cache<ChunkLba, Bytes>` — 64KB chunk granularity (16x fewer entries than per-block at 4K)
- `try_get_with()` — deduplicates concurrent misses (thundering herd protection)
- `Bytes` values — clone is O(1) refcount, slicing is zero-copy
- Default 128MB cache (configurable)

**Adaptive readahead (modeled after Linux `mm/readahead.c`):**
- Sequential detection: current read starts where last read ended
- Ramp-up: double window from 8 blocks (32KB) to 2048 blocks (8MB) on each sequential hit
- Async trigger: prefetch next window when reader crosses midpoint of current window
- Reset: random access or write resets window to minimum
- Prefetch runs as spawned tokio task, overlaps with consumption

**Cache invalidation:**
- Writes invalidate all overlapping chunks
- Readahead resets on write (breaks sequential pattern)

### 11. Block Device Layer (`block_device.rs`)

Channel-based dispatch from sync FUSE threads to async iSCSI pipeline.

**Architecture:**
- `BlockDevice` (FUSE-facing): `mpsc::Sender<BlockRequest>` handle. Clone + Send + Sync. Methods: `read_bytes()`, `write_bytes()`, `flush()` — all use `blocking_send()` / `blocking_recv()`.
- `BlockDeviceWorker` (async task): processes requests from channel with `tokio::select!` on request channel + coalesce timer.

**Write coalescing:**
- `DirtyMap` (BTreeMap by LBA): buffers writes, merges adjacent ranges
- Flush triggers: 5ms timer OR 1MB accumulated OR explicit fsync
- FUSE write returns after buffering (fast), SCSI WRITE on flush
- Read-your-writes: reads check dirty map before cache

**Bounded channel (256):** backpressure if FUSE generates faster than iSCSI processes.

### 12. FUSE Layer (`fuse_fs.rs`)

Multi-threaded FUSE, minimal change from current design.

- `n_threads = num_cpus::get()` — concurrent FUSE request handling
- `BlockDevice` is channel sender — safe to call from any thread, no `block_on()`
- `flush()` and `fsync()` both trigger dirty write flush
- Same single-file virtual filesystem: root dir + disk.img
- Same macFUSE volume/Finder integration

### 13. Main Integration (`main.rs`)

**Startup sequence:**
1. Parse CLI + load TOML config
2. Create tokio multi-thread runtime
3. TCP connect (4MB buffers, TCP_NODELAY, TCP keepalive)
4. iSCSI login (security + operational, CRC32C enabled)
5. Spawn response receiver task
6. READ CAPACITY (with UNIT ATTENTION retry)
7. Spawn NOP-Out keepalive task
8. Create moka cache (128MB, 64KB chunks, adaptive readahead)
9. Spawn block device worker (channel-based, write coalescing)
10. Mount multi-threaded FUSE (N = CPU count)
11. Block on FUSE event loop
12. On unmount: iSCSI logout + shutdown

## Dependencies

```toml
[dependencies]
fuser = "0.17"
num_cpus = "1"
tokio = { version = "1", features = ["full"] }
bytes = "1"
crc32c = "0.6"
socket2 = "0.6"
smallvec = "1"
moka = { version = "0.12", features = ["future"] }
toml = "0.8"
serde = { version = "1", features = ["derive"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
libc = "0.2"
ctrlc = "3"
dirs = "6"
```

**Removed:** `iscsi-client-rs`, `lru`, `tokio-util`

## Performance Targets

| Metric | Current | Target |
|--------|---------|--------|
| Sequential read | ~100 MB/s (estimated) | 1+ GB/s (10GbE line rate) |
| Sequential write | ~50 MB/s (estimated) | 800+ MB/s |
| FUSE threads | 1 | num_cpus |
| Outstanding SCSI commands | 1 | 128 |
| Cache hit cost | Vec clone (memcpy) | Bytes clone (refcount) |
| Cache size | 512KB-4MB | 128MB default |
| Readahead | None | Adaptive, up to 8MB |
| Write coalescing | None | 5ms / 1MB batching |
| MaxRecvDataSegmentLength | 262KB | 1MB |
| MaxBurstLength | 262KB | 1MB |
