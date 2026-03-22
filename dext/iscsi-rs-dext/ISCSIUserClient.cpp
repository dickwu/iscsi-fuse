#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>

#include "ISCSIUserClient.h"
#include "SharedMemory.h"

#define sUCLog OS_LOG_DEFAULT

struct ISCSIUserClient_IVars {
    IOService * provider = nullptr;
};

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

kern_return_t IMPL(ISCSIUserClient, Start) {
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) return ret;

    ivars->provider = provider;
    os_log(sUCLog, "ISCSIUserClient: Start: daemon connected");
    return kIOReturnSuccess;
}

kern_return_t IMPL(ISCSIUserClient, Stop) {
    os_log(sUCLog, "ISCSIUserClient: Stop: daemon disconnected");
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t ISCSIUserClient::ExternalMethod(
    uint64_t selector,
    IOUserClientMethodArguments * arguments,
    const IOUserClientMethodDispatch * dispatch,
    OSObject * target,
    void * reference)
{
    os_log(sUCLog, "ISCSIUserClient: ExternalMethod selector=%llu", selector);

    switch (selector) {
    case kSelectorSetGeometry:
        if (!arguments->scalarInput || arguments->scalarInputCount < 2) {
            return kIOReturnBadArgument;
        }
        os_log(sUCLog, "ISCSIUserClient: kSetGeometry: blockSize=%llu blockCount=%llu",
               arguments->scalarInput[0], arguments->scalarInput[1]);
        return kIOReturnSuccess;

    case kSelectorReadComplete:
    case kSelectorWriteComplete:
    case kSelectorSyncComplete:
        if (!arguments->scalarInput || arguments->scalarInputCount < 1) {
            return kIOReturnBadArgument;
        }
        os_log(sUCLog, "ISCSIUserClient: completion: selector=%llu reqId=%llu", selector, arguments->scalarInput[0]);
        return kIOReturnSuccess;

    default:
        os_log(sUCLog, "ISCSIUserClient: unknown selector %llu", selector);
        return kIOReturnBadArgument;
    }
}

kern_return_t IMPL(ISCSIUserClient, CopyClientMemoryForType) {
    os_log(sUCLog, "ISCSIUserClient: CopyClientMemoryForType type=%llu", type);
    // TODO: Return shared memory descriptors from ISCSIBlockStorageDevice
    // This requires cross-class access which will be implemented in Phase 4
    return kIOReturnUnsupported;
}
