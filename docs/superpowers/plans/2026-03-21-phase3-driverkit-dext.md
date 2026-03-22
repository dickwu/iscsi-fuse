# Phase 3: DriverKit Dext — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the DriverKit system extension (dext) that creates a real `/dev/diskN` block device on macOS. The dext is a thin proxy — it receives block I/O from the kernel and forwards it to the Rust daemon via shared memory + IOUserClient IPC.

**Architecture:** C++ DriverKit extension with two classes: `ISCSIBlockStorageDevice` (kernel-facing block device) and `ISCSIUserClient` (daemon-facing IPC). Shared memory regions for data transfer. IODataQueueDispatchSource for notifications.

**Tech Stack:** C++ (DriverKit), Xcode, IOUserBlockStorageDevice, IOUserClient, IOBufferMemoryDescriptor

**Spec:** `docs/superpowers/specs/2026-03-21-iscsi-rs-driverkit-design.md` (Phase 3, lines 509-521; Component Details, lines 86-231)

**Prerequisites:** Apple Developer account with DriverKit entitlements approved (requested in Phase 2).

---

## Important Notes for Implementation

- This phase creates the Xcode project and C++ code manually via Xcode GUI — it cannot be fully automated by an agent
- The agent can write the C++ source files, but Xcode project file (`.xcodeproj/project.pbxproj`) creation requires Xcode GUI
- The plan provides exact C++ code to write — copy it into files created by Xcode
- All testing uses `systemextensionsctl developer on` during development (no notarization needed yet)

---

## File Map — Target Structure

```
dext/
├── iscsi-rs-dext.xcodeproj/       (created by Xcode GUI)
├── iscsi-rs-dext/
│   ├── ISCSIBlockStorageDevice.iig (DriverKit interface definition)
│   ├── ISCSIBlockStorageDevice.cpp (block device implementation)
│   ├── ISCSIUserClient.iig        (user client interface)
│   ├── ISCSIUserClient.cpp        (user client implementation)
│   ├── SharedMemory.h             (shared memory structs + ring buffer)
│   ├── Info.plist                  (IOKit matching + bundle config)
│   └── Entitlements.plist          (DriverKit entitlements)
```

---

### Task 1: Create Xcode Project

This task MUST be done manually in Xcode. An agent cannot create `.xcodeproj` files.

- [ ] **Step 1: Create the DriverKit project in Xcode**

1. Open Xcode
2. File → New → Project
3. Choose: macOS → System Extension → DriverKit Driver
4. Product Name: `iscsi-rs-dext`
5. Team: your Apple Developer team
6. Organization Identifier: `com.peilinwu`
7. Bundle Identifier: `com.peilinwu.iscsi-rs-dext`
8. Language: C++
9. Save to: `/Users/peilinwu/project/iscsi/dext/`

- [ ] **Step 2: Add BlockStorageDeviceDriverKit framework**

In Xcode:
1. Select the target `iscsi-rs-dext`
2. General → Frameworks and Libraries → Add
3. Search for `BlockStorageDeviceDriverKit.framework` — add it
4. Also ensure `DriverKit.framework` is present

- [ ] **Step 3: Set deployment target**

1. Build Settings → macOS Deployment Target → 15.0

- [ ] **Step 4: Verify the project builds**

Run: `xcodebuild -project dext/iscsi-rs-dext.xcodeproj -scheme iscsi-rs-dext build` or use Xcode Build (Cmd+B)
Expected: Builds with default template code

- [ ] **Step 5: Commit the Xcode project**

```bash
git add dext/iscsi-rs-dext.xcodeproj dext/iscsi-rs-dext/
git commit -m "chore: create Xcode project for DriverKit dext"
```

---

### Task 2: Define Shared Memory Structures

**Files:**
- Create: `dext/iscsi-rs-dext/SharedMemory.h`

- [ ] **Step 1: Write the shared memory header**

Create `dext/iscsi-rs-dext/SharedMemory.h`:

```cpp
#ifndef SharedMemory_h
#define SharedMemory_h

#include <stdint.h>
#include <stdatomic.h>

// ---------------------------------------------------------------------------
// Memory region type IDs (used in CopyClientMemoryForType / IOConnectMapMemory64)
// ---------------------------------------------------------------------------

enum MemoryRegionType : uint64_t {
    kRegionControl       = 0,
    kRegionIORequestRing = 1,
    kRegionCompletionRing = 2,
    kRegionDataPool      = 3,
};

// ---------------------------------------------------------------------------
// Region 0: Control (4KB)
// ---------------------------------------------------------------------------

struct ControlRegion {
    // Geometry (set by daemon via kSetGeometry)
    uint32_t block_size;
    uint32_t _pad0;
    uint64_t block_count;

    // State
    uint8_t  attached;
    uint8_t  read_only;
    uint8_t  _pad1[2];
    uint32_t daemon_pid;
    uint64_t heartbeat_ts;

    // Stats
    uint64_t stat_reads;
    uint64_t stat_writes;
    uint64_t stat_errors;
};

static_assert(sizeof(ControlRegion) <= 4096, "Control region must fit in 4KB");

// ---------------------------------------------------------------------------
// I/O Operations
// ---------------------------------------------------------------------------

enum IOOperation : uint8_t {
    kIOOpRead  = 0,
    kIOOpWrite = 1,
    kIOOpSync  = 2,
};

// ---------------------------------------------------------------------------
// Region 1: I/O Request Ring (64KB)
// ---------------------------------------------------------------------------

struct IORequestEntry {
    uint32_t id;
    uint8_t  op;           // IOOperation
    uint8_t  _pad[3];      // align lba to 8 bytes
    uint64_t lba;
    uint32_t block_count;
    uint16_t buffer_idx;   // index into data pool
    uint8_t  _pad2[2];     // align entry to 24 bytes
};

static_assert(sizeof(IORequestEntry) == 24, "IORequestEntry must be 24 bytes");

static constexpr uint32_t kIORequestRingSize = 256;

struct IORequestRing {
    _Atomic(uint32_t) head;  // dext writes, daemon reads (release/acquire)
    _Atomic(uint32_t) tail;  // daemon writes, dext reads (release/acquire)
    IORequestEntry entries[kIORequestRingSize];
};

// ---------------------------------------------------------------------------
// Region 2: Completion Ring (16KB)
// ---------------------------------------------------------------------------

enum CompletionStatus : uint8_t {
    kCompletionOK    = 0,
    kCompletionError = 1,
};

struct CompletionEntry {
    uint32_t id;
    uint8_t  status;       // CompletionStatus
    uint8_t  _pad[3];
    uint32_t error_code;
    uint8_t  _pad2[4];     // align to 16 bytes
};

static_assert(sizeof(CompletionEntry) == 16, "CompletionEntry must be 16 bytes");

static constexpr uint32_t kCompletionRingSize = 256;

struct CompletionRing {
    _Atomic(uint32_t) head;  // daemon writes, dext reads
    _Atomic(uint32_t) tail;  // dext writes, daemon reads
    CompletionEntry entries[kCompletionRingSize];
};

// ---------------------------------------------------------------------------
// Region 3: Data Buffer Pool (32MB)
// ---------------------------------------------------------------------------

static constexpr uint32_t kDataBufferSlotSize  = 256 * 1024;  // 256KB per slot
static constexpr uint32_t kDataBufferSlotCount = 128;
static constexpr uint64_t kDataPoolSize = (uint64_t)kDataBufferSlotSize * kDataBufferSlotCount;

// ---------------------------------------------------------------------------
// IOUserClient selectors
// ---------------------------------------------------------------------------

enum ExternalMethodSelector : uint64_t {
    kSelectorSetGeometry   = 0,
    kSelectorReadComplete  = 1,
    kSelectorWriteComplete = 2,
    kSelectorSyncComplete  = 3,
};

#endif /* SharedMemory_h */
```

- [ ] **Step 2: Verify header compiles**

Add `#include "SharedMemory.h"` to the main dext source file and build.

- [ ] **Step 3: Commit**

```bash
git add dext/iscsi-rs-dext/SharedMemory.h
git commit -m "feat: add shared memory data structures for dext-daemon IPC

Defines ring buffer entries, control region, memory region types,
and IOUserClient selectors. All structs have explicit padding and
static_assert size checks for cross-language compatibility."
```

---

### Task 3: Implement ISCSIBlockStorageDevice

**Files:**
- Create/Modify: `dext/iscsi-rs-dext/ISCSIBlockStorageDevice.iig`
- Create/Modify: `dext/iscsi-rs-dext/ISCSIBlockStorageDevice.cpp`

- [ ] **Step 1: Write the .iig interface**

Create `dext/iscsi-rs-dext/ISCSIBlockStorageDevice.iig`:

```cpp
#ifndef ISCSIBlockStorageDevice_h
#define ISCSIBlockStorageDevice_h

#include <Availability.h>
#include <DriverKit/IOService.iig>
#include <BlockStorageDeviceDriverKit/IOUserBlockStorageDevice.iig>

class ISCSIBlockStorageDevice : public IOUserBlockStorageDevice
{
public:
    virtual bool init() override;
    virtual void free() override;

    virtual kern_return_t Start(IOService* provider) override;
    virtual kern_return_t Stop(IOService* provider) override;

    // Block device interface
    virtual kern_return_t doAsyncReadWrite(
        IOMemoryDescriptor* buffer,
        uint64_t block,
        uint64_t nblks,
        IOStorageAttributes* attributes,
        IOStorageCompletion* completion) override;

    virtual kern_return_t doSynchronize() override;
    virtual kern_return_t doEjectMedia() override;

    // Geometry reporting
    virtual kern_return_t ReportMaxReadTransfer(uint64_t blockSize,
                                                 uint64_t* max) override;
    virtual kern_return_t ReportMaxWriteTransfer(uint64_t blockSize,
                                                  uint64_t* max) override;
    virtual kern_return_t ReportMediumWritable(bool* isWritable) override;
    virtual kern_return_t ReportBlockSize(uint64_t* blockSize) override;
    virtual kern_return_t ReportMaxBlockCount(uint64_t* blockCount) override;
};

#endif /* ISCSIBlockStorageDevice_h */
```

Note: The exact .iig syntax depends on the Xcode DriverKit template. You may need to adjust based on what Xcode generates. The .iig file is an "Interface Implementation Generator" file that DriverKit uses to generate dispatch tables.

- [ ] **Step 2: Write the .cpp implementation**

Create `dext/iscsi-rs-dext/ISCSIBlockStorageDevice.cpp`:

```cpp
#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOMemoryDescriptor.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>

#include "ISCSIBlockStorageDevice.h"
#include "SharedMemory.h"

static os_log_t sLog = os_log_create("com.peilinwu.iscsi-rs", "BlockStorage");

struct ISCSIBlockStorageDevice_IVars {
    // Geometry (set via kSetGeometry from daemon)
    uint64_t blockSize  = 512;
    uint64_t blockCount = 0;
    bool     readOnly   = false;
    bool     geometrySet = false;

    // Shared memory regions
    IOBufferMemoryDescriptor* controlMem    = nullptr;
    IOBufferMemoryDescriptor* ioRingMem     = nullptr;
    IOBufferMemoryDescriptor* completionMem = nullptr;
    IOBufferMemoryDescriptor* dataPoolMem   = nullptr;

    // Typed pointers into shared memory
    ControlRegion*   control    = nullptr;
    IORequestRing*   ioRing     = nullptr;
    CompletionRing*  completion = nullptr;
    void*            dataPool   = nullptr;

    // Buffer slot bitmap (128 bits = 2 x uint64_t)
    uint64_t slotBitmap[2] = {0, 0};

    // Pending I/O completions (indexed by request ID)
    // For Phase 3, we use a simple array. Max 256 outstanding.
    struct PendingIO {
        IOMemoryDescriptor* buffer   = nullptr;
        IOStorageCompletion  completion = {};
        uint16_t             bufferIdx = 0;
        bool                 isRead    = false;
        bool                 active    = false;
    };
    PendingIO pendingIOs[kIORequestRingSize] = {};
    uint32_t  nextRequestId = 0;

    // Daemon connection state
    bool daemonConnected = false;
};

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

bool ISCSIBlockStorageDevice::init() {
    if (!super::init()) return false;
    ivars = IONewZero(ISCSIBlockStorageDevice_IVars, 1);
    if (!ivars) return false;
    os_log(sLog, "init");
    return true;
}

void ISCSIBlockStorageDevice::free() {
    os_log(sLog, "free");
    if (ivars) {
        OSSafeReleaseNULL(ivars->controlMem);
        OSSafeReleaseNULL(ivars->ioRingMem);
        OSSafeReleaseNULL(ivars->completionMem);
        OSSafeReleaseNULL(ivars->dataPoolMem);
        IODelete(ivars, ISCSIBlockStorageDevice_IVars, 1);
    }
    super::free();
}

kern_return_t ISCSIBlockStorageDevice::Start(IOService* provider) {
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) {
        os_log_error(sLog, "super::Start failed: 0x%x", ret);
        return ret;
    }

    os_log(sLog, "Start: allocating shared memory regions");

    // Allocate shared memory regions
    auto allocRegion = [](uint64_t size, IOBufferMemoryDescriptor** out) -> kern_return_t {
        return IOBufferMemoryDescriptor::Create(
            kIOMemoryDirectionInOut, size, 0, out);
    };

    ret = allocRegion(4096, &ivars->controlMem);
    if (ret != kIOReturnSuccess) { os_log_error(sLog, "control alloc failed"); return ret; }

    ret = allocRegion(sizeof(IORequestRing), &ivars->ioRingMem);
    if (ret != kIOReturnSuccess) { os_log_error(sLog, "io ring alloc failed"); return ret; }

    ret = allocRegion(sizeof(CompletionRing), &ivars->completionMem);
    if (ret != kIOReturnSuccess) { os_log_error(sLog, "completion ring alloc failed"); return ret; }

    ret = allocRegion(kDataPoolSize, &ivars->dataPoolMem);
    if (ret != kIOReturnSuccess) { os_log_error(sLog, "data pool alloc failed"); return ret; }

    // Map regions into dext address space
    uint64_t addr = 0, len = 0;
    IOMemoryMap* map = nullptr;

    ivars->controlMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { map->GetAddress(&addr); ivars->control = reinterpret_cast<ControlRegion*>(addr); OSSafeReleaseNULL(map); }

    ivars->ioRingMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { map->GetAddress(&addr); ivars->ioRing = reinterpret_cast<IORequestRing*>(addr); OSSafeReleaseNULL(map); }

    ivars->completionMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { map->GetAddress(&addr); ivars->completion = reinterpret_cast<CompletionRing*>(addr); OSSafeReleaseNULL(map); }

    ivars->dataPoolMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { map->GetAddress(&addr); ivars->dataPool = reinterpret_cast<void*>(addr); OSSafeReleaseNULL(map); }

    // Zero out rings
    if (ivars->ioRing) memset(ivars->ioRing, 0, sizeof(IORequestRing));
    if (ivars->completion) memset(ivars->completion, 0, sizeof(CompletionRing));
    if (ivars->control) memset(ivars->control, 0, sizeof(ControlRegion));

    os_log(sLog, "Start: shared memory allocated, waiting for daemon");

    // Note: we do NOT call RegisterService() here.
    // Device registration is deferred until daemon sends geometry via kSetGeometry.

    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::Stop(IOService* provider) {
    os_log(sLog, "Stop");
    return Stop(provider, SUPERDISPATCH);
}

// ---------------------------------------------------------------------------
// Geometry Reporting
// ---------------------------------------------------------------------------

kern_return_t ISCSIBlockStorageDevice::ReportBlockSize(uint64_t* blockSize) {
    *blockSize = ivars->blockSize;
    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::ReportMaxBlockCount(uint64_t* blockCount) {
    *blockCount = ivars->blockCount;
    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::ReportMaxReadTransfer(uint64_t blockSize, uint64_t* max) {
    *max = kDataBufferSlotSize;  // 256KB max per transfer
    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::ReportMaxWriteTransfer(uint64_t blockSize, uint64_t* max) {
    *max = kDataBufferSlotSize;  // 256KB max per transfer
    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::ReportMediumWritable(bool* isWritable) {
    *isWritable = !ivars->readOnly;
    return kIOReturnSuccess;
}

// ---------------------------------------------------------------------------
// Buffer Slot Management
// ---------------------------------------------------------------------------

static int allocSlot(uint64_t bitmap[2]) {
    for (int w = 0; w < 2; w++) {
        if (bitmap[w] != UINT64_MAX) {
            int bit = __builtin_ctzll(~bitmap[w]);
            bitmap[w] |= (1ULL << bit);
            return w * 64 + bit;
        }
    }
    return -1;  // all slots in use
}

static void freeSlot(uint64_t bitmap[2], int idx) {
    int w = idx / 64;
    int bit = idx % 64;
    bitmap[w] &= ~(1ULL << bit);
}

// ---------------------------------------------------------------------------
// Block I/O
// ---------------------------------------------------------------------------

kern_return_t ISCSIBlockStorageDevice::doAsyncReadWrite(
    IOMemoryDescriptor* buffer,
    uint64_t block,
    uint64_t nblks,
    IOStorageAttributes* attributes,
    IOStorageCompletion* completion)
{
    if (!ivars->daemonConnected || !ivars->geometrySet) {
        return kIOReturnNotReady;
    }

    // Determine if read or write from the buffer direction
    uint64_t bufLen = 0;
    buffer->GetLength(&bufLen);
    // The kernel sets the memory descriptor direction:
    // kIOMemoryDirectionIn = read (data flows IN to memory = read from disk)
    // kIOMemoryDirectionOut = write (data flows OUT of memory = write to disk)
    IOMemoryDescriptorDirection dir;
    buffer->GetDirection(&dir);
    bool isRead = (dir == kIOMemoryDirectionIn);

    // Allocate a data buffer slot
    int slotIdx = allocSlot(ivars->slotBitmap);
    if (slotIdx < 0) {
        os_log_error(sLog, "doAsyncReadWrite: no buffer slots available");
        return kIOReturnNoResources;
    }

    // For WRITE: copy data from kernel buffer to shared memory slot
    if (!isRead) {
        uint8_t* slotPtr = (uint8_t*)ivars->dataPool + (uint64_t)slotIdx * kDataBufferSlotSize;
        uint64_t copied = 0;
        buffer->ReadBytes(0, slotPtr, bufLen, &copied);
    }

    // Allocate request ID
    uint32_t reqId = ivars->nextRequestId++;

    // Store pending I/O
    uint32_t ringIdx = reqId % kIORequestRingSize;
    ivars->pendingIOs[ringIdx] = {
        .buffer     = buffer,
        .completion = *completion,
        .bufferIdx  = (uint16_t)slotIdx,
        .isRead     = isRead,
        .active     = true,
    };
    buffer->retain();

    // Enqueue to I/O request ring
    uint32_t head = atomic_load_explicit(&ivars->ioRing->head, memory_order_relaxed);
    uint32_t nextHead = (head + 1) % kIORequestRingSize;

    // Check if ring is full
    uint32_t tail = atomic_load_explicit(&ivars->ioRing->tail, memory_order_acquire);
    if (nextHead == tail) {
        freeSlot(ivars->slotBitmap, slotIdx);
        buffer->release();
        ivars->pendingIOs[ringIdx].active = false;
        os_log_error(sLog, "doAsyncReadWrite: I/O ring full");
        return kIOReturnNoResources;
    }

    IORequestEntry* entry = &ivars->ioRing->entries[head];
    entry->id = reqId;
    entry->op = isRead ? kIOOpRead : kIOOpWrite;
    entry->lba = block;
    entry->block_count = (uint32_t)nblks;
    entry->buffer_idx = (uint16_t)slotIdx;

    atomic_store_explicit(&ivars->ioRing->head, nextHead, memory_order_release);

    // Update stats
    if (isRead) ivars->control->stat_reads++;
    else ivars->control->stat_writes++;

    os_log_debug(sLog, "enqueued %s id=%u lba=%llu nblks=%llu slot=%d",
                 isRead ? "READ" : "WRITE", reqId, block, nblks, slotIdx);

    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::doSynchronize() {
    if (!ivars->daemonConnected) {
        return kIOReturnNotReady;
    }

    // Enqueue SYNC request (no buffer slot needed)
    uint32_t head = atomic_load_explicit(&ivars->ioRing->head, memory_order_relaxed);
    uint32_t nextHead = (head + 1) % kIORequestRingSize;
    uint32_t tail = atomic_load_explicit(&ivars->ioRing->tail, memory_order_acquire);
    if (nextHead == tail) {
        return kIOReturnNoResources;
    }

    uint32_t reqId = ivars->nextRequestId++;
    IORequestEntry* entry = &ivars->ioRing->entries[head];
    entry->id = reqId;
    entry->op = kIOOpSync;
    entry->lba = 0;
    entry->block_count = 0;
    entry->buffer_idx = 0;

    atomic_store_explicit(&ivars->ioRing->head, nextHead, memory_order_release);

    os_log_debug(sLog, "enqueued SYNC id=%u", reqId);
    return kIOReturnSuccess;
}

kern_return_t ISCSIBlockStorageDevice::doEjectMedia() {
    os_log(sLog, "doEjectMedia");
    return kIOReturnSuccess;
}
```

**Important:** The exact DriverKit API may differ based on macOS SDK version. Some methods may have slightly different signatures (e.g., `GetDirection` might be `GetFlags`). Check the BlockStorageDeviceDriverKit headers in your Xcode SDK when building. The `Start(provider, SUPERDISPATCH)` syntax is specific to DriverKit's dispatch model.

- [ ] **Step 3: Build and fix any compilation errors**

Build via Xcode (Cmd+B). DriverKit APIs may need adjustments based on the SDK version.

- [ ] **Step 4: Commit**

```bash
git add dext/iscsi-rs-dext/
git commit -m "feat: implement ISCSIBlockStorageDevice

IOUserBlockStorageDevice subclass with:
- Shared memory region allocation (control, I/O ring, completion ring, data pool)
- Buffer slot bitmap for 128x256KB data slots
- doAsyncReadWrite: enqueues to I/O ring, copies write data to shared slot
- doSynchronize: enqueues SYNC to I/O ring
- Geometry reporting (deferred until daemon sets via kSetGeometry)
- os_log diagnostics"
```

---

### Task 4: Implement ISCSIUserClient

**Files:**
- Create: `dext/iscsi-rs-dext/ISCSIUserClient.iig`
- Create: `dext/iscsi-rs-dext/ISCSIUserClient.cpp`

- [ ] **Step 1: Write the .iig interface**

Create `dext/iscsi-rs-dext/ISCSIUserClient.iig`:

```cpp
#ifndef ISCSIUserClient_h
#define ISCSIUserClient_h

#include <Availability.h>
#include <DriverKit/IOUserClient.iig>

class ISCSIBlockStorageDevice;

class ISCSIUserClient : public IOUserClient
{
public:
    virtual bool init() override;
    virtual void free() override;

    virtual kern_return_t Start(IOService* provider) override;
    virtual kern_return_t Stop(IOService* provider) override;

    virtual kern_return_t ExternalMethod(
        uint64_t selector,
        IOUserClientMethodArguments* arguments,
        const IOUserClientMethodDispatch* dispatch,
        OSObject* target,
        void* reference) override;

    virtual kern_return_t CopyClientMemoryForType(
        uint64_t type,
        uint64_t* options,
        IOMemoryDescriptor** memory) override;
};

#endif /* ISCSIUserClient_h */
```

- [ ] **Step 2: Write the .cpp implementation**

Create `dext/iscsi-rs-dext/ISCSIUserClient.cpp`:

```cpp
#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>

#include "ISCSIUserClient.h"
#include "ISCSIBlockStorageDevice.h"
#include "SharedMemory.h"

static os_log_t sUCLog = os_log_create("com.peilinwu.iscsi-rs", "UserClient");

struct ISCSIUserClient_IVars {
    ISCSIBlockStorageDevice* device = nullptr;
};

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

bool ISCSIUserClient::init() {
    if (!super::init()) return false;
    ivars = IONewZero(ISCSIUserClient_IVars, 1);
    return ivars != nullptr;
}

void ISCSIUserClient::free() {
    if (ivars) {
        IODelete(ivars, ISCSIUserClient_IVars, 1);
    }
    super::free();
}

kern_return_t ISCSIUserClient::Start(IOService* provider) {
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) return ret;

    // Get reference to our parent device
    ivars->device = OSDynamicCast(ISCSIBlockStorageDevice, provider);
    if (!ivars->device) {
        os_log_error(sUCLog, "Start: provider is not ISCSIBlockStorageDevice");
        return kIOReturnBadArgument;
    }

    os_log(sUCLog, "Start: daemon connected");
    // TODO: set device->ivars->daemonConnected = true
    // (requires friend access or a public method on ISCSIBlockStorageDevice)

    return kIOReturnSuccess;
}

kern_return_t ISCSIUserClient::Stop(IOService* provider) {
    os_log(sUCLog, "Stop: daemon disconnected");
    // TODO: set device->ivars->daemonConnected = false
    return Stop(provider, SUPERDISPATCH);
}

// ---------------------------------------------------------------------------
// ExternalMethod — daemon-to-dext commands
// ---------------------------------------------------------------------------

kern_return_t ISCSIUserClient::ExternalMethod(
    uint64_t selector,
    IOUserClientMethodArguments* arguments,
    const IOUserClientMethodDispatch* dispatch,
    OSObject* target,
    void* reference)
{
    switch (selector) {
    case kSelectorSetGeometry: {
        // Daemon sends block_size (scalar[0]) and block_count (scalar[1])
        if (!arguments->scalarInput || arguments->scalarInputCount < 2) {
            return kIOReturnBadArgument;
        }
        uint64_t blockSize  = arguments->scalarInput[0];
        uint64_t blockCount = arguments->scalarInput[1];

        os_log(sUCLog, "kSetGeometry: blockSize=%llu blockCount=%llu", blockSize, blockCount);

        // TODO: Store geometry on device and call RegisterService()
        // ivars->device->setGeometry(blockSize, blockCount);

        return kIOReturnSuccess;
    }

    case kSelectorReadComplete:
    case kSelectorWriteComplete:
    case kSelectorSyncComplete: {
        // Daemon signals that a completion is ready in the completion ring
        // scalar[0] = request ID
        if (!arguments->scalarInput || arguments->scalarInputCount < 1) {
            return kIOReturnBadArgument;
        }
        uint32_t reqId = (uint32_t)arguments->scalarInput[0];

        os_log_debug(sUCLog, "completion signal: selector=%llu reqId=%u", selector, reqId);

        // TODO: Process completion from the completion ring
        // - Dequeue completion entry
        // - For READ: copy data from shared buffer to kernel IOMemoryDescriptor
        // - Call IOStorage::Complete() on the pending IOStorageCompletion
        // - Free buffer slot

        return kIOReturnSuccess;
    }

    default:
        os_log_error(sUCLog, "ExternalMethod: unknown selector %llu", selector);
        return kIOReturnBadArgument;
    }
}

// ---------------------------------------------------------------------------
// CopyClientMemoryForType — daemon maps shared memory regions
// ---------------------------------------------------------------------------

kern_return_t ISCSIUserClient::CopyClientMemoryForType(
    uint64_t type,
    uint64_t* options,
    IOMemoryDescriptor** memory)
{
    if (!ivars->device) {
        return kIOReturnNotReady;
    }

    os_log(sUCLog, "CopyClientMemoryForType: type=%llu", type);

    // TODO: Return the appropriate IOBufferMemoryDescriptor based on type
    // switch (type) {
    //   case kRegionControl:        *memory = device->ivars->controlMem; break;
    //   case kRegionIORequestRing:  *memory = device->ivars->ioRingMem; break;
    //   case kRegionCompletionRing: *memory = device->ivars->completionMem; break;
    //   case kRegionDataPool:       *memory = device->ivars->dataPoolMem; break;
    // }
    // (*memory)->retain();

    return kIOReturnUnsupported;  // TODO: implement after resolving cross-class access
}
```

**Note:** The TODOs in this implementation are intentional — cross-class access between ISCSIUserClient and ISCSIBlockStorageDevice's ivars requires either friend declarations, public accessor methods, or restructuring. This will be refined when the actual Xcode build reveals the DriverKit patterns needed.

- [ ] **Step 3: Build and iterate**

Build via Xcode. Fix compilation errors. DriverKit has specific patterns for cross-class communication that may require adjustments.

- [ ] **Step 4: Commit**

```bash
git add dext/iscsi-rs-dext/
git commit -m "feat: implement ISCSIUserClient for daemon IPC

IOUserClient subclass with:
- ExternalMethod dispatch for kSetGeometry, kReadComplete,
  kWriteComplete, kSyncComplete selectors
- CopyClientMemoryForType for shared memory mapping
- TODOs for cross-class access patterns"
```

---

### Task 5: Configure Info.plist and Entitlements

**Files:**
- Modify: `dext/iscsi-rs-dext/Info.plist`
- Create/Modify: `dext/iscsi-rs-dext/Entitlements.plist`

- [ ] **Step 1: Update Info.plist with IOKit matching**

Add to the `IOKitPersonalities` dictionary in `Info.plist`:

```xml
<key>IOKitPersonalities</key>
<dict>
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
        <string>com.peilinwu.iscsi-rs-dext</string>
    </dict>
</dict>
```

Also add IOUserClientClass for the user client:

```xml
<key>IOUserClientClass</key>
<string>ISCSIUserClient</string>
```

- [ ] **Step 2: Set entitlements**

Ensure `Entitlements.plist` contains:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.developer.driverkit</key>
    <true/>
    <key>com.apple.developer.driverkit.family.block-storage-device</key>
    <true/>
    <key>com.apple.developer.driverkit.transport.userclient</key>
    <true/>
</dict>
</plist>
```

- [ ] **Step 3: Build**

Build via Xcode. Resolve any plist or entitlement issues.

- [ ] **Step 4: Commit**

```bash
git add dext/iscsi-rs-dext/Info.plist dext/iscsi-rs-dext/Entitlements.plist
git commit -m "feat: configure IOKit matching and DriverKit entitlements

IOUserResources matching for software-only block device.
Entitlements for block-storage-device and userclient."
```

---

### Task 6: Test Dext Loading

- [ ] **Step 1: Enable developer mode for system extensions**

```bash
systemextensionsctl developer on
```

This requires SIP adjustments on Apple Silicon — follow Apple's documentation.

- [ ] **Step 2: Build and install the dext**

Build in Xcode with your Development signing identity. The dext should be embedded in an app or installed manually.

- [ ] **Step 3: Verify dext loads**

Check system extension status:
```bash
systemextensionsctl list
```

Check for the dext in IORegistry:
```bash
ioreg -l | grep -i iscsi
```

- [ ] **Step 4: Write a minimal test client (Rust or C)**

Create a simple test that:
1. Calls `IOServiceGetMatchingService()` to find the dext
2. Calls `IOServiceOpen()` to create a user client connection
3. Calls `IOConnectMapMemory64()` for each shared memory region (types 0-3)
4. Verifies the mapped addresses are non-null
5. Calls `IOServiceClose()`

This validates the Phase 3 gate: "a test client can open IOUserClient and map shared memory regions."

- [ ] **Step 5: Commit test client**

```bash
git add tests/ crates/
git commit -m "test: add dext loading and IOUserClient connection test"
```

---

## Phase 3 Gate

All of these must be true before proceeding to Phase 4:

- [ ] Xcode project builds with no errors
- [ ] Dext loads as a system extension (appears in `systemextensionsctl list`)
- [ ] Dext creates `/dev/diskN` when geometry is set (may require hardcoded geometry for testing)
- [ ] A test client can open IOUserClient connection to the dext
- [ ] A test client can map all 4 shared memory regions via `IOConnectMapMemory64`
- [ ] Entitlements file has all 3 required keys
- [ ] Info.plist has IOUserResources matching configured
