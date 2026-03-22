#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOMemoryDescriptor.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/IOMemoryMap.h>

#include "ISCSIBlockStorageDevice.h"
#include "SharedMemory.h"

#define sLog OS_LOG_DEFAULT

struct ISCSIBlockStorageDevice_IVars {
    uint64_t blockSize   = 512;
    uint64_t blockCount  = 0;
    bool     readOnly    = false;
    bool     geometrySet = false;
    bool     daemonConnected = false;

    IOBufferMemoryDescriptor * controlMem    = nullptr;
    IOBufferMemoryDescriptor * ioRingMem     = nullptr;
    IOBufferMemoryDescriptor * completionMem = nullptr;
    IOBufferMemoryDescriptor * dataPoolMem   = nullptr;

    ControlRegion  * control    = nullptr;
    IORequestRing  * ioRing     = nullptr;
    CompletionRing * completion = nullptr;
    void           * dataPool   = nullptr;

    uint64_t slotBitmap[2] = {0, 0};
    uint32_t nextRequestId = 0;
};

bool ISCSIBlockStorageDevice::init() {
    if (!super::init()) return false;
    ivars = IONewZero(ISCSIBlockStorageDevice_IVars, 1);
    if (!ivars) return false;
    os_log(sLog, "ISCSIBlockStorageDevice::init");
    return true;
}

void ISCSIBlockStorageDevice::free() {
    os_log(sLog, "ISCSIBlockStorageDevice::free");
    if (ivars) {
        OSSafeReleaseNULL(ivars->controlMem);
        OSSafeReleaseNULL(ivars->ioRingMem);
        OSSafeReleaseNULL(ivars->completionMem);
        OSSafeReleaseNULL(ivars->dataPoolMem);
        IODelete(ivars, ISCSIBlockStorageDevice_IVars, 1);
    }
    super::free();
}

kern_return_t IMPL(ISCSIBlockStorageDevice, Start) {
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) {
        os_log(sLog, "ISCSIBlockStorageDevice: super::Start failed: 0x%x", ret);
        return ret;
    }

    os_log(sLog, "ISCSIBlockStorageDevice: Start: allocating shared memory regions");

    // Allocate shared memory
    ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, 4096, 0, &ivars->controlMem);
    if (ret != kIOReturnSuccess) { os_log(sLog, "ISCSIBlockStorageDevice: control alloc failed"); return ret; }

    ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, sizeof(IORequestRing), 0, &ivars->ioRingMem);
    if (ret != kIOReturnSuccess) { os_log(sLog, "ISCSIBlockStorageDevice: io ring alloc failed"); return ret; }

    ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, sizeof(CompletionRing), 0, &ivars->completionMem);
    if (ret != kIOReturnSuccess) { os_log(sLog, "ISCSIBlockStorageDevice: completion ring alloc failed"); return ret; }

    ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, kDataPoolSize, 0, &ivars->dataPoolMem);
    if (ret != kIOReturnSuccess) { os_log(sLog, "ISCSIBlockStorageDevice: data pool alloc failed"); return ret; }

    // Map into dext address space
    IOMemoryMap * map = nullptr;
    uint64_t addr = 0;

    ivars->controlMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { addr = map->GetAddress(); ivars->control = reinterpret_cast<ControlRegion *>(addr); OSSafeReleaseNULL(map); }

    ivars->ioRingMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { addr = map->GetAddress(); ivars->ioRing = reinterpret_cast<IORequestRing *>(addr); OSSafeReleaseNULL(map); }

    ivars->completionMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { addr = map->GetAddress(); ivars->completion = reinterpret_cast<CompletionRing *>(addr); OSSafeReleaseNULL(map); }

    ivars->dataPoolMem->CreateMapping(0, 0, 0, 0, 0, &map);
    if (map) { addr = map->GetAddress(); ivars->dataPool = reinterpret_cast<void *>(addr); OSSafeReleaseNULL(map); }

    if (ivars->control) memset(ivars->control, 0, sizeof(ControlRegion));
    if (ivars->ioRing) memset(ivars->ioRing, 0, sizeof(IORequestRing));
    if (ivars->completion) memset(ivars->completion, 0, sizeof(CompletionRing));

    os_log(sLog, "ISCSIBlockStorageDevice: Start: shared memory allocated, waiting for daemon");
    return kIOReturnSuccess;
}

kern_return_t IMPL(ISCSIBlockStorageDevice, Stop) {
    os_log(sLog, "ISCSIBlockStorageDevice::Stop");
    return Stop(provider, SUPERDISPATCH);
}
