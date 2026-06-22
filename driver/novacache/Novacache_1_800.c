/*=====================================================================
  Novacache.c  –  Mini-filter driver skeleton
=====================================================================*/

#include "Novacache.h"

/*-------------------------------------------------
   Instance context – we keep simple counters per
   filter instance (i.e. per mounted volume)
-------------------------------------------------*/
typedef struct _NOVACACHE_INSTANCE_CONTEXT {
    ULONG64 ReadOps;
    ULONG64 WriteOps;
    ULONG32 VolumeId;
    WCHAR DriveLetter;
} NOVACACHE_INSTANCE_CONTEXT, *PNOVACACHE_INSTANCE_CONTEXT;

/*-------------------------------------------------
   Global data
-------------------------------------------------*/
PFLT_FILTER gFilterHandle = NULL;
PFLT_PORT gServerPort     = NULL;    // The user-mode communication server port

// Shared Memory State
HANDLE gSharedMemSectionHandle = NULL;
PVOID gSharedMemSectionObject = NULL;
volatile PVOID gSharedMemView = NULL;
volatile PSHARED_MEM_HEADER gSharedMemHeader = NULL;
volatile PSHARED_MEM_BLOCK_DESC gSharedMemDescriptors = NULL;
volatile PUCHAR gSharedMemData = NULL;
volatile PCACHE_DIRECTORY_ENTRY gCacheDirectory = NULL;
volatile PUCHAR gCacheData = NULL;
PKEVENT gSharedMemEvent = NULL;
HANDLE gSharedMemEventHandle = NULL;
LONG gSharedMemRefAndState = 0;
SIZE_T gSharedMemViewSize = 0;
EX_PUSH_LOCK gRingBufferLock;
extern POBJECT_TYPE *MmSectionObjectType;

/*---------
    }

    if (!NovacacheIsCacheableFileObject(FltObjects)) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    // We only process reads
    if (Data->Iopb->MajorFunction == IRP_MJ_READ) {
        PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
        NTSTATUS ctxStatus = FltGetInstanceContext(FltObjects->Instance, (PFLT_CONTEXT *)&ctx);
        ULONG32 volId = 0;
        if (NT_SUCCESS(ctxStatus) && ctx != NULL) {
            volId = ctx->VolumeId;
            FltReleaseContext(ctx);
        }

        if (!IsVolumeEnabled(volId)) {
            return FLT_POSTOP_FINISHED_PROCESSING;
        }

        if (AcquireSharedMemReference()) {
            PVOID buffer = NULL;
            ULONG length = (ULONG)Data->IoStatus.Information;

            // Try to get a valid buffer pointer from MdlAddress.
            // If MdlAddress is NULL, we can safely use ReadBuffer ONLY if it is a system-space address.
            if (Data->Iopb->Parameters.Read.MdlAddress != NULL) {
                buffer = MmGetSystemAddressForMdlSafe(Data->Iopb->Parameters.Read.MdlAddress, NormalPagePriority);
            } else if (Data->Iopb->Parameters.Read.ReadBuffer != NULL) {
                PVOID rawBuffer = Data->Iopb->Parameters.Read.ReadBuffer;
                if ((ULONG_PTR)rawBuffer >= MmUserProbeAddress) {
                    buffer = rawBuffer;
                }
            }

            if (buffer != NULL) {
                ULONG64 offset = Data->Iopb->Parameters.Read.ByteOffset.QuadPart;

                if (CompletionContext != NULL &&
                    length > 0 &&
                    length <= CACHE_BLOCK_SIZE &&