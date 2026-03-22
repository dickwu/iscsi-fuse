#ifndef SharedMemory_h
#define SharedMemory_h

#include <stdint.h>

// Memory region type IDs for CopyClientMemoryForType / IOConnectMapMemory64
enum MemoryRegionType : uint64_t {
    kRegionControl        = 0,
    kRegionIORequestRing  = 1,
    kRegionCompletionRing = 2,
    kRegionDataPool       = 3,
};

// Control region (4KB)
struct ControlRegion {
    uint32_t block_size;
    uint32_t _pad0;
    uint64_t block_count;
    uint8_t  attached;
    uint8_t  read_only;
    uint8_t  _pad1[2];
    uint32_t daemon_pid;
    uint64_t heartbeat_ts;
    uint64_t stat_reads;
    uint64_t stat_writes;
    uint64_t stat_errors;
};

// I/O operations
enum IOOperation : uint8_t {
    kIOOpRead  = 0,
    kIOOpWrite = 1,
    kIOOpSync  = 2,
};

// I/O request ring entry (24 bytes, explicit padding)
struct IORequestEntry {
    uint32_t id;
    uint8_t  op;
    uint8_t  _pad[3];
    uint64_t lba;
    uint32_t block_count;
    uint16_t buffer_idx;
    uint8_t  _pad2[2];
};

static_assert(sizeof(IORequestEntry) == 24, "IORequestEntry must be 24 bytes");

static constexpr uint32_t kIORequestRingSize = 256;

struct IORequestRing {
    uint32_t head;  // dext writes, daemon reads
    uint32_t tail;  // daemon writes, dext reads
    IORequestEntry entries[kIORequestRingSize];
};

// Completion ring entry (16 bytes)
enum CompletionStatus : uint8_t {
    kCompletionOK    = 0,
    kCompletionError = 1,
};

struct CompletionEntry {
    uint32_t id;
    uint8_t  status;
    uint8_t  _pad[3];
    uint32_t error_code;
    uint8_t  _pad2[4];
};

static_assert(sizeof(CompletionEntry) == 16, "CompletionEntry must be 16 bytes");

static constexpr uint32_t kCompletionRingSize = 256;

struct CompletionRing {
    uint32_t head;
    uint32_t tail;
    CompletionEntry entries[kCompletionRingSize];
};

// Data buffer pool
static constexpr uint32_t kDataBufferSlotSize  = 256 * 1024;  // 256KB
static constexpr uint32_t kDataBufferSlotCount = 128;
static constexpr uint64_t kDataPoolSize = (uint64_t)kDataBufferSlotSize * kDataBufferSlotCount;

// IOUserClient selectors
enum ExternalMethodSelector : uint64_t {
    kSelectorSetGeometry   = 0,
    kSelectorReadComplete  = 1,
    kSelectorWriteComplete = 2,
    kSelectorSyncComplete  = 3,
};

#endif
