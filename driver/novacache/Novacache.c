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

HANDLE gL2CacheFileHandle = NULL;
PFILE_OBJECT gL2CacheFileObject = NULL;

volatile PVOID gSharedMemView = NULL;
volatile PSHARED_MEM_HEADER gSharedMemHeader = NULL;
volatile PSHARED_MEM_BLOCK_DESC gSharedMemDescriptors = NULL;
volatile PUCHAR gSharedMemData = NULL;
volatile PCACHE_DIRECTORY_ENTRY gCacheDirectory = NULL;
volatile PCACHE_DIRECTORY_ENTRY gL2CacheDirectory = NULL;
volatile PUCHAR gCacheData = NULL;
PKEVENT gSharedMemEvent = NULL;
HANDLE gSharedMemEventHandle = NULL;
LONG gSharedMemRefAndState = 0;
SIZE_T gSharedMemViewSize = 0;
EX_PUSH_LOCK gRingBufferLock;
extern POBJECT_TYPE *MmSectionObjectType;

#ifndef NOVACACHE_TAG
#define NOVACACHE_TAG 'CvoN'
#endif

// Dynamically use the block size provided by the user-mode service
#define CACHE_BLOCK_SIZE (gSharedMemHeader->BlockSize)

VOID NovacacheWriteToSharedRing(ULONG32 VolumeId, ULONG64 Offset, ULONG32 Length, ULONG32 Flags, ULONG64 PreOpTick, ULONG64 PostOpTick, PVOID FileObject, PVOID Data);

NTSTATUS DriverEntry(PDRIVER_OBJECT DriverObject, PUNICODE_STRING RegistryPath);
NTSTATUS NovacacheInstanceSetup(PCFLT_RELATED_OBJECTS FltObjects, FLT_INSTANCE_SETUP_FLAGS Flags, DEVICE_TYPE VolumeDeviceType, FLT_FILESYSTEM_TYPE VolumeFilesystemType);
VOID NovacacheInstanceTeardownStart(PCFLT_RELATED_OBJECTS FltObjects, FLT_INSTANCE_TEARDOWN_FLAGS Flags);
VOID NovacacheInstanceTeardownComplete(PCFLT_RELATED_OBJECTS FltObjects, FLT_INSTANCE_TEARDOWN_FLAGS Flags);
NTSTATUS NovacacheUnload(FLT_FILTER_UNLOAD_FLAGS Flags);
VOID NovacacheInstanceContextCleanup(PFLT_CONTEXT Context, FLT_CONTEXT_TYPE ContextType);
VOID NovacacheStreamContextCleanup(PFLT_CONTEXT Context, FLT_CONTEXT_TYPE ContextType);
FLT_POSTOP_CALLBACK_STATUS NovacachePostOperationCreate(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID CompletionContext, FLT_POST_OPERATION_FLAGS Flags);
FLT_PREOP_CALLBACK_STATUS NovacachePreOperationReadWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID *CompletionContext);
FLT_POSTOP_CALLBACK_STATUS NovacachePostOperationReadWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID CompletionContext, FLT_POST_OPERATION_FLAGS Flags);
FLT_POSTOP_CALLBACK_STATUS NovacacheSafePostOperationReadWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID CompletionContext, FLT_POST_OPERATION_FLAGS Flags);
FLT_PREOP_CALLBACK_STATUS NovacachePreOperationWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID *CompletionContext);
FLT_POSTOP_CALLBACK_STATUS NovacachePostOperationWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID CompletionContext, FLT_POST_OPERATION_FLAGS Flags);
FLT_POSTOP_CALLBACK_STATUS NovacacheSafePostOperationWrite(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID CompletionContext, FLT_POST_OPERATION_FLAGS Flags);
FLT_PREOP_CALLBACK_STATUS NovacachePreOperationSectionSync(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID *CompletionContext);
FLT_PREOP_CALLBACK_STATUS NovacachePreOperationSetInfo(PFLT_CALLBACK_DATA Data, PCFLT_RELATED_OBJECTS FltObjects, PVOID *CompletionContext);
NTSTATUS NovacachePortConnect(PFLT_PORT ClientPort, PVOID ServerPortCookie, PVOID ConnectionContext, ULONG SizeOfContext, PVOID *ConnectionPortCookie);
VOID NovacachePortDisconnect(PVOID ConnectionCookie);
NTSTATUS NovacachePortMessage(PVOID PortCookie, PVOID InputBuffer, ULONG InputBufferLength, PVOID OutputBuffer, ULONG OutputBufferLength, PULONG ReturnOutputBufferLength);

NTSTATUS
NovacacheGetFileId(
    _In_ PFLT_INSTANCE Instance,
    _In_ PFILE_OBJECT FileObject,
    _In_ BOOLEAN QueryIfMissing,
    _Out_ PULONG64 FileId
    );

typedef struct _NOVACACHE_STREAM_CONTEXT {
    ULONG64 FileId;
    ULONG32 VolumeId;
    BOOLEAN DeleteOnClose;
} NOVACACHE_STREAM_CONTEXT, *PNOVACACHE_STREAM_CONTEXT;


const FLT_CONTEXT_REGISTRATION ContextRegistration[] = {
    { FLT_INSTANCE_CONTEXT,
      0,
      NovacacheInstanceContextCleanup,
      sizeof(NOVACACHE_INSTANCE_CONTEXT),
      NOVACACHE_TAG },
    { FLT_STREAM_CONTEXT,
      0,
      NovacacheStreamContextCleanup,
      sizeof(NOVACACHE_STREAM_CONTEXT),
      NOVACACHE_TAG },
    { FLT_CONTEXT_END }
};

const FLT_OPERATION_REGISTRATION Callbacks[] = {
    { IRP_MJ_CREATE,
      0,
      NULL,
      NovacachePostOperationCreate },
    { IRP_MJ_READ,
      0,
      NovacachePreOperationReadWrite,
      NovacachePostOperationReadWrite },
    { IRP_MJ_WRITE,
      0,
      NovacachePreOperationWrite,
      NovacachePostOperationWrite },
    { IRP_MJ_ACQUIRE_FOR_SECTION_SYNCHRONIZATION,
      0,
      NovacachePreOperationSectionSync,
      NULL },
    { IRP_MJ_SET_INFORMATION,
      0,
      NovacachePreOperationSetInfo,
      NULL },
    { IRP_MJ_OPERATION_END }
};

const FLT_REGISTRATION FilterRegistration = {
    sizeof( FLT_REGISTRATION ),
    FLT_REGISTRATION_VERSION,
    0,                       // Flags
    ContextRegistration,     // Context
    Callbacks,               // Operation callbacks
    NovacacheUnload,         // FilterUnload
    NovacacheInstanceSetup,  // InstanceSetup
    NULL,                    // InstanceQueryTeardown
    NovacacheInstanceTeardownStart, // InstanceTeardownStart
    NovacacheInstanceTeardownComplete, // InstanceTeardownComplete
    NULL,                    // GenerateFileName
    NULL,                    // NormalizeNameComponent
    NULL                     // NormalizeContextCleanup
};

#define ACTIVE_BIT   0x80000000

BOOLEAN AcquireSharedMemReference(VOID) {
    LONG state = InterlockedIncrement(&gSharedMemRefAndState);
    if ((state & ACTIVE_BIT) == 0) {
        InterlockedDecrement(&gSharedMemRefAndState);
        return FALSE;
    }
    return TRUE;
}

VOID ReleaseSharedMemReference(VOID) {
    InterlockedDecrement(&gSharedMemRefAndState);
}

BOOLEAN IsVolumeEnabled(ULONG32 volId) {
    if (AcquireSharedMemReference()) {
        BOOLEAN enabled = FALSE;
        if (gSharedMemHeader) {
            if (volId < 32) {
                enabled = (gSharedMemHeader->VolumeBitmap & (1 << volId)) != 0;
            }
        }
        ReleaseSharedMemReference();
        return enabled;
    }
    return FALSE;
}

BOOLEAN NovacacheIsCacheableFileObject(PCFLT_RELATED_OBJECTS FltObjects) {
    if (FltObjects->FileObject == NULL) return FALSE;

    PFILE_OBJECT fo = FltObjects->FileObject;

    if (FlagOn(fo->Flags, FO_VOLUME_OPEN | FO_DIRECT_DEVICE_OPEN)) {
        return FALSE;
    }

    if (KeGetCurrentIrql() == PASSIVE_LEVEL) {
        if (fo->FileName.Length == 0 || fo->FileName.Buffer == NULL) {
            return FALSE;
        }

        PWCH buf = fo->FileName.Buffer;
        USHORT len = fo->FileName.Length / sizeof(WCHAR);
        for (USHORT i = 0; i < len; i++) {
            if (buf[i] == L'$') {
                return FALSE;
            }
        }

        USHORT lastSep = 0;
        for (USHORT i = len; i > 0; i--) {
            if (buf[i - 1] == L'\\' || buf[i - 1] == L'/') {
                lastSep = i;
                break;
            }
        }
        PWCH fname = buf + lastSep;
        USHORT fnameLen = len - lastSep;

        if (fnameLen > 0) {
            UNICODE_STRING name;
            name.Buffer = fname;
            name.Length = (USHORT)(fnameLen * sizeof(WCHAR));
            name.MaximumLength = name.Length;

            UNICODE_STRING pf, hf, sf;
            RtlInitUnicodeString(&pf, L"pagefile.sys");
            RtlInitUnicodeString(&hf, L"hiberfil.sys");
            RtlInitUnicodeString(&sf, L"swapfile.sys");

            if (RtlEqualUnicodeString(&name, &pf, TRUE) ||
                RtlEqualUnicodeString(&name, &hf, TRUE) ||
                RtlEqualUnicodeString(&name, &sf, TRUE)) {
                return FALSE;
            }
        }
    }

    return TRUE;
}

NTSTATUS DriverEntry(PDRIVER_OBJECT DriverObject, PUNICODE_STRING RegistryPath) {
    UNREFERENCED_PARAMETER(RegistryPath);
    NTSTATUS status;

    ExInitializePushLock(&gRingBufferLock);

    status = FltRegisterFilter(DriverObject, &FilterRegistration, &gFilterHandle);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    UNICODE_STRING portName;
    OBJECT_ATTRIBUTES oa;
    PSECURITY_DESCRIPTOR sd;

    status = FltBuildDefaultSecurityDescriptor(&sd, FLT_PORT_ALL_ACCESS);
    if (NT_SUCCESS(status)) {
        RtlInitUnicodeString(&portName, L"\\NovaCachePort");
        InitializeObjectAttributes(&oa, &portName, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, sd);

        status = FltCreateCommunicationPort(
            gFilterHandle,
            &gServerPort,
            &oa,
            NULL,
            NovacachePortConnect,
            NovacachePortDisconnect,
            NovacachePortMessage,
            1
        );
        FltFreeSecurityDescriptor(sd);
    }

    if (!NT_SUCCESS(status)) {
        FltUnregisterFilter(gFilterHandle);
        return status;
    }

    status = FltStartFiltering(gFilterHandle);
    if (!NT_SUCCESS(status)) {
        FltCloseCommunicationPort(gServerPort);
        FltUnregisterFilter(gFilterHandle);
    }

    return status;
}

NTSTATUS NovacacheUnload(FLT_FILTER_UNLOAD_FLAGS Flags) {
    UNREFERENCED_PARAMETER(Flags);

    if (gServerPort) {
        FltCloseCommunicationPort(gServerPort);
        gServerPort = NULL;
    }
    if (gFilterHandle) {
        FltUnregisterFilter(gFilterHandle);
        gFilterHandle = NULL;
    }
    return STATUS_SUCCESS;
}

NTSTATUS NovacacheInstanceSetup(
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ FLT_INSTANCE_SETUP_FLAGS Flags,
    _In_ DEVICE_TYPE VolumeDeviceType,
    _In_ FLT_FILESYSTEM_TYPE VolumeFilesystemType
) {
    UNREFERENCED_PARAMETER(Flags);
    UNREFERENCED_PARAMETER(VolumeDeviceType);
    UNREFERENCED_PARAMETER(VolumeFilesystemType);

    // L2 cache file opening has been moved to NovacachePortConnect

    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS status = FltAllocateContext(FltObjects->Filter, FLT_INSTANCE_CONTEXT, sizeof(NOVACACHE_INSTANCE_CONTEXT), NonPagedPool, (PFLT_CONTEXT *)&ctx);
    if (NT_SUCCESS(status)) {
        RtlZeroMemory(ctx, sizeof(NOVACACHE_INSTANCE_CONTEXT));
        
        ctx->VolumeId = 2; // Default to C: (index 2)
        ctx->DriveLetter = L'C';
        
        PDEVICE_OBJECT devObj = NULL;
        status = FltGetDiskDeviceObject(FltObjects->Volume, &devObj);
        if (NT_SUCCESS(status) && devObj != NULL) {
            UNICODE_STRING dosName;
            status = IoVolumeDeviceToDosName(devObj, &dosName);
            if (NT_SUCCESS(status)) {
                USHORT len = dosName.Length / sizeof(WCHAR);
                for (USHORT i = 0; i < len; i++) {
                    if (dosName.Buffer[i] == L':' && i > 0) {
                        WCHAR ch = dosName.Buffer[i - 1];
                        if ((ch >= L'A' && ch <= L'Z') || (ch >= L'a' && ch <= L'z')) {
                            ctx->DriveLetter = ch;
                            if (ch >= L'A' && ch <= L'Z') {
                                ctx->VolumeId = (ULONG32)(ch - L'A');
                            } else {
                                ctx->VolumeId = (ULONG32)(ch - L'a');
                            }
                            KdPrint(("Novacache: Auto-detected VolumeId=%lu, DriveLetter=%wc for device %wZ\n", 
                                     ctx->VolumeId, ctx->DriveLetter, &dosName));
                        }
                        break;
                    }
                }
                ExFreePool(dosName.Buffer);
            }
            ObDereferenceObject(devObj);
        }
        
        status = FltSetInstanceContext(FltObjects->Instance, FLT_SET_CONTEXT_KEEP_IF_EXISTS, ctx, NULL);
        FltReleaseContext(ctx);
        return STATUS_SUCCESS;
    }
    return status;
}

VOID NovacacheInstanceTeardownStart(PCFLT_RELATED_OBJECTS FltObjects, FLT_INSTANCE_TEARDOWN_FLAGS Flags) {
    UNREFERENCED_PARAMETER(FltObjects);
    UNREFERENCED_PARAMETER(Flags);
}

VOID NovacacheInstanceTeardownComplete(PCFLT_RELATED_OBJECTS FltObjects, FLT_INSTANCE_TEARDOWN_FLAGS Flags) {
    UNREFERENCED_PARAMETER(FltObjects);
    UNREFERENCED_PARAMETER(Flags);
}

VOID NovacacheInstanceContextCleanup(PFLT_CONTEXT Context, FLT_CONTEXT_TYPE ContextType) {
    UNREFERENCED_PARAMETER(Context);
    UNREFERENCED_PARAMETER(ContextType);
}

VOID NovacacheStreamContextCleanup(PFLT_CONTEXT Context, FLT_CONTEXT_TYPE ContextType) {
    UNREFERENCED_PARAMETER(ContextType);
    PNOVACACHE_STREAM_CONTEXT streamCtx = (PNOVACACHE_STREAM_CONTEXT)Context;

    if (streamCtx != NULL && streamCtx->DeleteOnClose) {
        if (AcquireSharedMemReference()) {
            ULONG32 capacity = gSharedMemHeader->Capacity;
            ULONG32 volId = streamCtx->VolumeId;
            PVOID fileObject = (PVOID)streamCtx->FileId;

            if (capacity > 0 && gCacheDirectory != NULL) {
                ULONG32 invalidated = 0;

                for (ULONG32 i = 0; i < capacity && invalidated < 256; i++) {
                    if (gCacheDirectory[i].Valid == 1 &&
                        gCacheDirectory[i].VolumeId == volId &&
                        gCacheDirectory[i].FileObject == fileObject)
                    {
                        InterlockedExchange((LONG *)&gCacheDirectory[i].Valid, 0);
                        invalidated++;
                    }
                }

                ULONG32 l2Capacity = gSharedMemHeader->L2Capacity;
                PCACHE_DIRECTORY_ENTRY l2Dir = gL2CacheDirectory;
                if (l2Capacity > 0 && l2Dir != NULL) {
                    for (ULONG32 i = 0; i < l2Capacity; i++) {
                        if (l2Dir[i].Valid == 1 &&
                            l2Dir[i].VolumeId == volId &&
                            l2Dir[i].FileObject == fileObject)
                        {
                            InterlockedExchange((LONG *)&l2Dir[i].Valid, 0);
                            invalidated++;
                        }
                    }
                }

                UCHAR dummyData = 0;
                NovacacheWriteToSharedRing(volId, 0, 0, SHARED_MEM_FLAG_INVALIDATE, 0, 0, fileObject, &dummyData);

                KdPrint(("Novacache: StreamContextCleanup invalidation volId=%u total_invalidated=%lu\n",
                         volId, invalidated));
            }
            ReleaseSharedMemReference();
        }
    }
}

NTSTATUS
NovacacheGetFileId(
    _In_ PFLT_INSTANCE Instance,
    _In_ PFILE_OBJECT FileObject,
    _In_ BOOLEAN QueryIfMissing,
    _Out_ PULONG64 FileId
    )
{
    NTSTATUS status;
    PNOVACACHE_STREAM_CONTEXT streamCtx = NULL;

    if (Instance == NULL || FileObject == NULL || FileId == NULL) {
        return STATUS_INVALID_PARAMETER;
    }

    // 1. Try to get existing stream context
    status = FltGetStreamContext(Instance, FileObject, (PFLT_CONTEXT *)&streamCtx);
    if (NT_SUCCESS(status) && streamCtx != NULL) {
        *FileId = streamCtx->FileId;
        FltReleaseContext(streamCtx);
        return STATUS_SUCCESS;
    }

    // 2. Query File ID from the filesystem only if permitted and running at PASSIVE_LEVEL
    if (QueryIfMissing && KeGetCurrentIrql() == PASSIVE_LEVEL) {
        FILE_INTERNAL_INFORMATION fileInfo;
        status = FltQueryInformationFile(
            Instance,
            FileObject,
            &fileInfo,
            sizeof(fileInfo),
            FileInternalInformation,
            NULL
        );
        if (NT_SUCCESS(status)) {
            *FileId = (ULONG64)fileInfo.IndexNumber.QuadPart;

            // 3. Try to allocate and associate stream context
            NTSTATUS allocStatus = FltAllocateContext(
                gFilterHandle,
                FLT_STREAM_CONTEXT,
                sizeof(NOVACACHE_STREAM_CONTEXT),
                NonPagedPool,
                (PFLT_CONTEXT *)&streamCtx
            );
            if (NT_SUCCESS(allocStatus) && streamCtx != NULL) {
                streamCtx->FileId = *FileId;
                
                PNOVACACHE_INSTANCE_CONTEXT ic = NULL;
                if (NT_SUCCESS(FltGetInstanceContext(Instance, (PFLT_CONTEXT *)&ic)) && ic != NULL) {
                    streamCtx->VolumeId = ic->VolumeId;
                    FltReleaseContext(ic);
                } else {
                    streamCtx->VolumeId = 0;
                }
                streamCtx->DeleteOnClose = FALSE;

                status = FltSetStreamContext(
                    Instance,
                    FileObject,
                    FLT_SET_CONTEXT_KEEP_IF_EXISTS,
                    streamCtx,
                    NULL
                );
                FltReleaseContext(streamCtx);
            }
            return STATUS_SUCCESS;
        }
    }

    // 4. Fallback if query is not permitted, fails, or if IRQL is too high
    return STATUS_NOT_FOUND;
}

FLT_POSTOP_CALLBACK_STATUS
NovacachePostOperationCreate(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags
    )
{
    UNREFERENCED_PARAMETER(CompletionContext);

    if (Flags & FLTFL_POST_OPERATION_DRAINING) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NT_SUCCESS(Data->IoStatus.Status) || Data->IoStatus.Information == FILE_DOES_NOT_EXIST) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NovacacheIsCacheableFileObject(FltObjects)) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    // Warm up and cache the File ID at PASSIVE_LEVEL on file creation/open
    ULONG64 fileId = 0;
    if (NT_SUCCESS(NovacacheGetFileId(FltObjects->Instance, FltObjects->FileObject, TRUE, &fileId))) {
        ULONG createOptions = Data->Iopb->Parameters.Create.Options;
        if (FlagOn(createOptions, FILE_DELETE_ON_CLOSE)) {
            PNOVACACHE_STREAM_CONTEXT streamCtx = NULL;
            if (NT_SUCCESS(FltGetStreamContext(FltObjects->Instance, FltObjects->FileObject, (PFLT_CONTEXT *)&streamCtx)) && streamCtx != NULL) {
                streamCtx->DeleteOnClose = TRUE;
                FltReleaseContext(streamCtx);
            }
        }

        ULONG info = (ULONG)Data->IoStatus.Information;
        if (info == FILE_SUPERSEDED || info == FILE_OVERWRITTEN) {
            PNOVACACHE_INSTANCE_CONTEXT ic = NULL;
            if (NT_SUCCESS(FltGetInstanceContext(FltObjects->Instance, (PFLT_CONTEXT *)&ic)) && ic != NULL) {
                ULONG32 volId = ic->VolumeId;
                FltReleaseContext(ic);

                if (AcquireSharedMemReference()) {
                    ULONG32 capacity = gSharedMemHeader->Capacity;
                    PVOID fileObject = (PVOID)fileId;
                    ULONG32 invalidated = 0;

                    if (capacity > 0 && gCacheDirectory != NULL) {
                        for (ULONG32 i = 0; i < capacity && invalidated < 256; i++) {
                            if (gCacheDirectory[i].Valid == 1 &&
                                gCacheDirectory[i].VolumeId == volId &&
                                gCacheDirectory[i].FileObject == fileObject)
                            {
                                InterlockedExchange((LONG *)&gCacheDirectory[i].Valid, 0);
                                invalidated++;
                            }
                        }
                    }

                    ULONG32 l2Capacity = gSharedMemHeader->L2Capacity;
                    PCACHE_DIRECTORY_ENTRY l2Dir = gL2CacheDirectory;
                    if (l2Capacity > 0 && l2Dir != NULL) {
                        for (ULONG32 i = 0; i < l2Capacity; i++) {
                            if (l2Dir[i].Valid == 1 &&
                                l2Dir[i].VolumeId == volId &&
                                l2Dir[i].FileObject == fileObject)
                            {
                                InterlockedExchange((LONG *)&l2Dir[i].Valid, 0);
                                invalidated++;
                            }
                        }
                    }

                    UCHAR dummyData = 0;
                    NovacacheWriteToSharedRing(volId, 0, 0, SHARED_MEM_FLAG_INVALIDATE, 0, 0, fileObject, &dummyData);

                    KdPrint(("Novacache: CREATE overwrite invalidation volId=%u info=%u total_invalidated=%lu\n",
                             volId, info, invalidated));
                    ReleaseSharedMemReference();
                }
            }
        }
    }

    return FLT_POSTOP_FINISHED_PROCESSING;
}

FLT_PREOP_CALLBACK_STATUS
NovacachePreOperationReadWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_    PCFLT_RELATED_OBJECTS FltObjects,
    _Out_   PVOID *CompletionContext
    )
{
    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS status;

    status = FltGetInstanceContext(FltObjects->Instance,
                                   (PFLT_CONTEXT *)&ctx);
    if (!NT_SUCCESS(status) || ctx == NULL) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    ULONG32 volId = ctx->VolumeId;

    if (Data->Iopb->MajorFunction == IRP_MJ_READ) {
        if (!NovacacheIsCacheableFileObject(FltObjects)) {
            InterlockedIncrement64(&ctx->ReadOps);
            FltReleaseContext(ctx);
            return FLT_PREOP_SUCCESS_NO_CALLBACK;
        }

        // Skip paging I/O to prevent deadlocks and page faults in paging paths (shared memory is pageable)
        if (FlagOn(Data->Iopb->IrpFlags, IRP_PAGING_IO)) {
            InterlockedIncrement64(&ctx->ReadOps);
            FltReleaseContext(ctx);
            return FLT_PREOP_SUCCESS_NO_CALLBACK;
        }

        ULONG64 offset = Data->Iopb->Parameters.Read.ByteOffset.QuadPart;
        ULONG length = Data->Iopb->Parameters.Read.Length;

        // Skip caching for disabled volumes
        if (!IsVolumeEnabled(volId)) {
            InterlockedIncrement64(&ctx->ReadOps);
            FltReleaseContext(ctx);
            return FLT_PREOP_SUCCESS_NO_CALLBACK;
        }

        if (length > 0) {
            // Lock user buffer if it's a user-mode address and we don't have MDL yet, so it is safe in PostOp
            if (FLT_IS_IRP_OPERATION(Data)) {
                if (Data->Iopb->Parameters.Read.MdlAddress == NULL) {
                    PVOID rawBuffer = Data->Iopb->Parameters.Read.ReadBuffer;
                    if (rawBuffer != NULL && (ULONG_PTR)rawBuffer < MmUserProbeAddress) {
                        __try {
                            FltLockUserBuffer(Data);
                        } __except (EXCEPTION_EXECUTE_HANDLER) {
                            KdPrint(("Novacache: Exception in FltLockUserBuffer: 0x%08x\n", GetExceptionCode()));
                        }
                    }
                }
            }

            LARGE_INTEGER perfFreq;
            LARGE_INTEGER preOpTick = KeQueryPerformanceCounter(&perfFreq);
            *CompletionContext = (PVOID)preOpTick.QuadPart;

            if (AcquireSharedMemReference()) {
                PSHARED_MEM_HEADER hdr = gSharedMemHeader;
                PCACHE_DIRECTORY_ENTRY dir = gCacheDirectory;
                PUCHAR cacheData = gCacheData;

                if (hdr && dir && cacheData) {
                    ULONG32 capacity = hdr->Capacity;

                    if (capacity > 0) {
                        ULONG32 numBuckets = capacity / 2;
                        PVOID fileObject = NULL;
                        ULONG64 fileId = 0;
                        if (!NT_SUCCESS(NovacacheGetFileId(FltObjects->Instance, FltObjects->FileObject, TRUE, &fileId))) {
                            FltReleaseContext(ctx);
                            return FLT_PREOP_SUCCESS_NO_CALLBACK;
                        }
                        fileObject = (PVOID)fileId;

                        if (numBuckets > 0) {
                            ULONG64 startChunk = offset / CACHE_BLOCK_SIZE;
                            ULONG64 endChunk = (offset + length - 1) / CACHE_BLOCK_SIZE;
                            ULONG numChunks = (ULONG)(endChunk - startChunk + 1);
                            BOOLEAN allChunksHit = TRUE;
                            
                            for (ULONG i = 0; i < numChunks; i++) {
                                ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                                ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                ULONG32 slotA = bucket * 2;
                                ULONG32 slotB = bucket * 2 + 1;
                                
                                ULONG64 chunkStartInRead = 0;
                                if (chunkOffset > offset) {
                                    chunkStartInRead = chunkOffset - offset;
                                }
                                ULONG64 chunkDataOffset = 0;
                                if (offset > chunkOffset) {
                                    chunkDataOffset = offset - chunkOffset;
                                }
                                ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                                if (chunkStartInRead + copyLen > length) {
                                    copyLen = length - chunkStartInRead;
                                }
                                
                                ULONG64 exactStart = offset + chunkStartInRead;
                                ULONG32 exactLen = (ULONG32)copyLen;
                                
                                LONG hitWay = -1;
                                if (slotA < capacity && dir[slotA].Valid == 1 && dir[slotA].VolumeId == volId && dir[slotA].FileObject == fileObject && 
                                    dir[slotA].Offset <= exactStart && (dir[slotA].Offset + dir[slotA].Length) >= (exactStart + exactLen)) {
                                    hitWay = 0;
                                } else if (slotB < capacity && dir[slotB].Valid == 1 && dir[slotB].VolumeId == volId && dir[slotB].FileObject == fileObject &&
                                    dir[slotB].Offset <= exactStart && (dir[slotB].Offset + dir[slotB].Length) >= (exactStart + exactLen)) {
                                    hitWay = 1;
                                }
                                
                                if (hitWay < 0) {
                                    allChunksHit = FALSE;
                                    break;
                                }
                            }
                            
                            BOOLEAN allChunksInL1OrL2 = TRUE;
                            ULONG32 l2Capacity = hdr->L2Capacity;
                            PCACHE_DIRECTORY_ENTRY l2Dir = gL2CacheDirectory;
                            
                            ULONG32 chunkSlots[16]; 
                            if (numChunks > 16) {
                                allChunksInL1OrL2 = FALSE;
                            }
                            
                            if (!allChunksHit && allChunksInL1OrL2 && gL2CacheFileObject && l2Dir && l2Capacity > 0) {
                                ULONG32 numL2Buckets = l2Capacity / 4;
                                if (numL2Buckets > 0) {
                                    for (ULONG i = 0; i < numChunks; i++) {
                                        ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                                        
                                        ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                        ULONG32 slotA = bucket * 2;
                                        ULONG32 slotB = bucket * 2 + 1;
                                        
                                        ULONG64 chunkStartInRead = 0;
                                        if (chunkOffset > offset) {
                                            chunkStartInRead = chunkOffset - offset;
                                        }
                                        ULONG64 chunkDataOffset = 0;
                                        if (offset > chunkOffset) {
                                            chunkDataOffset = offset - chunkOffset;
                                        }
                                        ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                                        if (chunkStartInRead + copyLen > length) {
                                            copyLen = length - chunkStartInRead;
                                        }
                                        
                                        ULONG64 exactStart = offset + chunkStartInRead;
                                        ULONG32 exactLen = (ULONG32)copyLen;
                                        
                                        LONG hitWay = -1;
                                        if (slotA < capacity && dir[slotA].Valid == 1 && dir[slotA].VolumeId == volId && dir[slotA].FileObject == fileObject &&
                                            dir[slotA].Offset <= exactStart && (dir[slotA].Offset + dir[slotA].Length) >= (exactStart + exactLen)) {
                                            hitWay = 0;
                                            chunkSlots[i] = slotA;
                                        } else if (slotB < capacity && dir[slotB].Valid == 1 && dir[slotB].VolumeId == volId && dir[slotB].FileObject == fileObject &&
                                            dir[slotB].Offset <= exactStart && (dir[slotB].Offset + dir[slotB].Length) >= (exactStart + exactLen)) {
                                            hitWay = 1;
                                            chunkSlots[i] = slotB;
                                        }
                                        
                                        if (hitWay < 0) {
                                            ULONG32 l2Bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numL2Buckets);
                                            ULONG32 baseSlot = l2Bucket * 4;
                                            LONG l2HitWay = -1;
                                            for (ULONG j = 0; j < 4; j++) {
                                                ULONG32 s = baseSlot + j;
                                                if (s < l2Capacity && l2Dir[s].Valid == 1 && l2Dir[s].VolumeId == volId && l2Dir[s].FileObject == fileObject &&
                                                    l2Dir[s].Offset <= exactStart && (l2Dir[s].Offset + l2Dir[s].Length) >= (exactStart + exactLen)) {
                                                    l2HitWay = j;
                                                    chunkSlots[i] = 0x80000000 | s; 
                                                    break;
                                                }
                                            }
                                            if (l2HitWay < 0) {
                                                allChunksInL1OrL2 = FALSE;
                                                break;
                                            }
                                        }
                                    }
                                } else {
                                    allChunksInL1OrL2 = FALSE;
                                }
                            } else {
                                allChunksInL1OrL2 = allChunksHit && (numChunks <= 16);
                                if (allChunksInL1OrL2) {
                                    for (ULONG i = 0; i < numChunks; i++) {
                                        ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                                        ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                        ULONG32 slotA = bucket * 2;
                                        ULONG32 slotB = bucket * 2 + 1;
                                        ULONG64 chunkStartInRead = 0;
                                        if (chunkOffset > offset) {
                                            chunkStartInRead = chunkOffset - offset;
                                        }
                                        ULONG64 chunkDataOffset = 0;
                                        if (offset > chunkOffset) {
                                            chunkDataOffset = offset - chunkOffset;
                                        }
                                        ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                                        if (chunkStartInRead + copyLen > length) {
                                            copyLen = length - chunkStartInRead;
                                        }
                                        
                                        ULONG64 exactStart = offset + chunkStartInRead;
                                        ULONG32 exactLen = (ULONG32)copyLen;
                                        if (slotA < capacity && dir[slotA].Valid == 1 && dir[slotA].VolumeId == volId && dir[slotA].FileObject == fileObject &&
                                            dir[slotA].Offset <= exactStart && (dir[slotA].Offset + dir[slotA].Length) >= (exactStart + exactLen)) {
                                            chunkSlots[i] = slotA;
                                        } else {
                                            chunkSlots[i] = slotB;
                                        }
                                    }
                                }
                            }
                            
                            if (allChunksInL1OrL2) {
                                // CRITICAL FIX: We must not return more data than the actual file size (EOF),
                                // otherwise we return garbage memory and crash applications (e.g. Unity games).
                                BOOLEAN safeToComplete = TRUE;
                                if (KeGetCurrentIrql() == PASSIVE_LEVEL) {
                                    FILE_STANDARD_INFORMATION stdInfo;
                                    NTSTATUS qStatus = FltQueryInformationFile(
                                        FltObjects->Instance,
                                        FltObjects->FileObject,
                                        &stdInfo,
                                        sizeof(stdInfo),
                                        FileStandardInformation,
                                        NULL
                                    );
                                    if (NT_SUCCESS(qStatus)) {
                                        ULONG64 eof = stdInfo.EndOfFile.QuadPart;
                                        if (offset >= eof) {
                                            // Read at or past EndOfFile
                                            Data->IoStatus.Status = STATUS_END_OF_FILE;
                                            Data->IoStatus.Information = 0;
                                            FltReleaseContext(ctx);
                                            return FLT_PREOP_COMPLETE;
                                        }
                                        if (offset + length > eof) {
                                            length = (ULONG)(eof - offset);
                                        }
                                    } else {
                                        safeToComplete = FALSE;
                                    }
                                } else {
                                    safeToComplete = FALSE;
                                }

                                if (!safeToComplete) {
                                    allChunksInL1OrL2 = FALSE;
                                }
                            }
                            
                            if (allChunksInL1OrL2) {
                                PVOID userBuffer = NULL;
                                if (FLT_IS_IRP_OPERATION(Data)) {
                                    if (Data->Iopb->Parameters.Read.MdlAddress != NULL) {
                                        userBuffer = MmGetSystemAddressForMdlSafe(Data->Iopb->Parameters.Read.MdlAddress, NormalPagePriority | MdlMappingNoExecute);
                                    } else {
                                        PVOID rawBuffer = Data->Iopb->Parameters.Read.ReadBuffer;
                                        if (rawBuffer != NULL && (ULONG_PTR)rawBuffer >= MmUserProbeAddress) {
                                            userBuffer = rawBuffer;
                                        }
                                    }
                                } else {
                                    userBuffer = Data->Iopb->Parameters.Read.ReadBuffer;
                                }
                                
                                if (userBuffer != NULL) {
                                    BOOLEAN copySuccess = TRUE;
                                    ULONG bytesCopied = 0;
                                    
                                    for (ULONG i = 0; i < numChunks; i++) {
                                        ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                                        ULONG32 slotId = chunkSlots[i];
                                        BOOLEAN isL2 = (slotId & 0x80000000) != 0;
                                        ULONG32 slot = slotId & ~0x80000000;
                                        
                                        PCACHE_DIRECTORY_ENTRY targetDir = isL2 ? l2Dir : dir;
                                        
                                        volatile ULONG64 seqBefore = targetDir[slot].SequenceNum;
                                        
                                        ULONG64 chunkStartInRead = 0;
                                        if (chunkOffset > offset) {
                                            chunkStartInRead = chunkOffset - offset;
                                        }
                                        ULONG64 chunkDataOffset = 0;
                                        if (offset > chunkOffset) {
                                            chunkDataOffset = offset - chunkOffset;
                                        }
                                        ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                                        if (chunkStartInRead + copyLen > length) {
                                            copyLen = length - chunkStartInRead;
                                        }
                                        
                                        ULONG64 exactStart = offset + chunkStartInRead;
                                        ULONG32 exactLen = (ULONG32)copyLen;
                                        __try {
                                            if (isL2) {
                                                ULONG32 l2_slot_index = targetDir[slot].SlotIndex;
                                                LARGE_INTEGER readOffset;
                                                readOffset.QuadPart = (LONGLONG)l2_slot_index * CACHE_BLOCK_SIZE + (exactStart % CACHE_BLOCK_SIZE);
                                                IO_STATUS_BLOCK ioStatus;
                                                
                                                NTSTATUS readStatus = ZwReadFile(gL2CacheFileHandle, NULL, NULL, NULL, &ioStatus, 
                                                    (PUCHAR)userBuffer + chunkStartInRead, (ULONG)copyLen, &readOffset, NULL);
                                                
                                                if (!NT_SUCCESS(readStatus) || ioStatus.Information != copyLen) {
                                                    copySuccess = FALSE;
                                                    break;
                                                }
                                            } else {
                                                PUCHAR src = cacheData + ((ULONG64)slot * CACHE_BLOCK_SIZE) + (exactStart % CACHE_BLOCK_SIZE);
                                                RtlCopyMemory((PUCHAR)userBuffer + chunkStartInRead, src, (SIZE_T)copyLen);
                                            }
                                            KeMemoryBarrier();
                                            volatile ULONG64 seqAfter = targetDir[slot].SequenceNum;
                                            if (seqBefore != seqAfter || targetDir[slot].Valid == 0 || targetDir[slot].VolumeId != volId || targetDir[slot].FileObject != fileObject ||
                                                targetDir[slot].Offset > exactStart || (targetDir[slot].Offset + targetDir[slot].Length) < (exactStart + exactLen)) {
                                                copySuccess = FALSE;
                                                break;
                                            }
                                            bytesCopied += (ULONG)copyLen;
                                        } __except (EXCEPTION_EXECUTE_HANDLER) {
                                            copySuccess = FALSE;
                                            break;
                                        }
                                    }
                                    
                                    if (copySuccess && bytesCopied == length) {
                                        InterlockedAdd64(&gSharedMemHeader->CachedHits, numChunks);
                                        Data->IoStatus.Status = STATUS_SUCCESS;
                                        Data->IoStatus.Information = length;

                                        FltReleaseContext(ctx);
                                        ReleaseSharedMemReference();
                                        return FLT_PREOP_COMPLETE;
                                    }
                                }
                            }
                        }
                    }
                }
                ReleaseSharedMemReference();
            }

            InterlockedIncrement64(&ctx->ReadOps);
            FltReleaseContext(ctx);

            return FLT_PREOP_SUCCESS_WITH_CALLBACK;
        }

        InterlockedIncrement64(&ctx->ReadOps);
        FltReleaseContext(ctx);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    FltReleaseContext(ctx);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
}

FLT_POSTOP_CALLBACK_STATUS
NovacachePostOperationReadWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags
    )
{
    if (Flags & FLTFL_POST_OPERATION_DRAINING) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NT_SUCCESS(Data->IoStatus.Status) || Data->IoStatus.Information == 0) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NovacacheIsCacheableFileObject(FltObjects)) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (Data->Iopb->MajorFunction == IRP_MJ_READ) {
        if (!FLT_IS_IRP_OPERATION(Data)) {
            return NovacacheSafePostOperationReadWrite(Data, FltObjects, CompletionContext, Flags);
        }

        if (KeGetCurrentIrql() == DISPATCH_LEVEL) {
            FLT_POSTOP_CALLBACK_STATUS retStatus;
            if (FltDoCompletionProcessingWhenSafe(
                    Data,
                    FltObjects,
                    CompletionContext,
                    Flags,
                    NovacacheSafePostOperationReadWrite,
                    &retStatus
                ))
            {
                return retStatus;
            }
            return FLT_POSTOP_FINISHED_PROCESSING;
        } else {
            return NovacacheSafePostOperationReadWrite(Data, FltObjects, CompletionContext, Flags);
        }
    }

    return FLT_POSTOP_FINISHED_PROCESSING;
}

FLT_POSTOP_CALLBACK_STATUS
NovacacheSafePostOperationReadWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags
    )
{
    UNREFERENCED_PARAMETER(FltObjects);
    UNREFERENCED_PARAMETER(Flags);

    if (!NT_SUCCESS(Data->IoStatus.Status) || Data->IoStatus.Information == 0) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    PVOID fileObject = NULL;
    ULONG64 fileId = 0;
    if (Data->Iopb->TargetFileObject == NULL) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    if (!NT_SUCCESS(NovacacheGetFileId(Data->Iopb->TargetInstance, Data->Iopb->TargetFileObject, TRUE, &fileId))) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    fileObject = (PVOID)fileId;

    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS ctxStatus = FltGetInstanceContext(Data->Iopb->TargetInstance, (PFLT_CONTEXT *)&ctx);
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

        if (Data->Iopb->Parameters.Read.MdlAddress != NULL) {
            buffer = MmGetSystemAddressForMdlSafe(Data->Iopb->Parameters.Read.MdlAddress, NormalPagePriority | MdlMappingNoExecute);
        } else if (Data->Iopb->Parameters.Read.ReadBuffer != NULL) {
            PVOID rawBuffer = Data->Iopb->Parameters.Read.ReadBuffer;
            if (!FLT_IS_IRP_OPERATION(Data)) {
                // FastIo: always in original thread context, safe to use user buffer
                buffer = rawBuffer;
            } else {
                // IRP without MDL: only safe if it's a kernel buffer.
                // User-mode buffers are dangerous here because PostOp can run in an arbitrary thread context (e.g. System process).
                if ((ULONG_PTR)rawBuffer >= MmUserProbeAddress) {
                    buffer = rawBuffer;
                }
            }
        }

        if (buffer != NULL) {
            ULONG64 offset = Data->Iopb->Parameters.Read.ByteOffset.QuadPart;

            if (CompletionContext != NULL && length > 0) {
                LARGE_INTEGER perfFreq;
                LARGE_INTEGER postOpTick = KeQueryPerformanceCounter(&perfFreq);

                ULONG64 startChunk = offset / CACHE_BLOCK_SIZE;
                ULONG64 endChunk = (offset + length - 1) / CACHE_BLOCK_SIZE;
                ULONG numChunks = (ULONG)(endChunk - startChunk + 1);

                for (ULONG i = 0; i < numChunks; i++) {
                    ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                    
                    ULONG64 chunkStartInRead = 0;
                    if (chunkOffset > offset) {
                        chunkStartInRead = chunkOffset - offset;
                    }
                    
                    ULONG64 chunkDataOffset = 0;
                    if (offset > chunkOffset) {
                        chunkDataOffset = offset - chunkOffset;
                    }
                    
                    ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                    if (chunkStartInRead + copyLen > length) {
                        copyLen = length - chunkStartInRead;
                    }

                    if (copyLen > 0) {
                        BOOLEAN isCached = FALSE;
                        PSHARED_MEM_HEADER hdr = gSharedMemHeader;
                        PCACHE_DIRECTORY_ENTRY dir = gCacheDirectory;
                        if (hdr && dir) {
                            ULONG32 capacity = hdr->Capacity;
                            if (capacity > 0) {
                                ULONG32 numBuckets = capacity / 2;
                                if (numBuckets > 0) {
                                    ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                    ULONG32 slotA = bucket * 2;
                                    ULONG32 slotB = bucket * 2 + 1;
                                    
                                    ULONG64 exactStart = offset + chunkStartInRead;
                                    ULONG32 exactLen = (ULONG32)copyLen;
                                    if (slotA < capacity && dir[slotA].Valid == 1 && dir[slotA].VolumeId == volId && dir[slotA].FileObject == fileObject &&
                                        dir[slotA].Offset <= exactStart && (dir[slotA].Offset + dir[slotA].Length) >= (exactStart + exactLen)) {
                                        isCached = TRUE;
                                    } else if (slotB < capacity && dir[slotB].Valid == 1 && dir[slotB].VolumeId == volId && dir[slotB].FileObject == fileObject &&
                                        dir[slotB].Offset <= exactStart && (dir[slotB].Offset + dir[slotB].Length) >= (exactStart + exactLen)) {
                                        isCached = TRUE;
                                    }
                                }
                            }
                        }

                        if (!isCached) {
                            __try {
                                NovacacheWriteToSharedRing(volId, offset + chunkStartInRead, (ULONG32)copyLen, 0,
                                                           (ULONG64)CompletionContext, postOpTick.QuadPart, 
                                                           fileObject, (PUCHAR)buffer + chunkStartInRead);
                                InterlockedIncrement64(&gSharedMemHeader->CachedReadsTotal);
                            } __except (EXCEPTION_EXECUTE_HANDLER) {
                                KdPrint(("Novacache: Exception in PostOp ring write: 0x%08x\n", GetExceptionCode()));
                            }
                        }
                    }
                }
            }
        }
        ReleaseSharedMemReference();
    }

    return FLT_POSTOP_FINISHED_PROCESSING;
}

/*-------------------------------------------------
   Helper – collect stats from a given instance context
-------------------------------------------------*/
NTSTATUS
NovacacheGetStats(
    _In_ PNOVACACHE_INSTANCE_CONTEXT InstanceCtx,
    _Out_ PNOVACACHE_STATS Stats
    )
{
    if (!InstanceCtx || !Stats)
        return STATUS_INVALID_PARAMETER;

    Stats->ReadOperations  = InstanceCtx->ReadOps;
    Stats->WriteOperations = InstanceCtx->WriteOps;
    return STATUS_SUCCESS;
}

VOID WriteStatusToFile(NTSTATUS openStatus, NTSTATUS mapStatus) {
    UNICODE_STRING fileName;
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK ioStatus;
    HANDLE fileHandle;
    NTSTATUS status;
    struct {
        NTSTATUS OpenStatus;
        NTSTATUS MapStatus;
    } data;

    RtlInitUnicodeString(&fileName, L"\\??\\E:\\Desktop\\Scripts\\Nova Cache\\driver_status.bin");
    InitializeObjectAttributes(&oa, &fileName, OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE, NULL, NULL);

    status = ZwCreateFile(&fileHandle,
                          FILE_APPEND_DATA | SYNCHRONIZE,
                          &oa,
                          &ioStatus,
                          NULL,
                          FILE_ATTRIBUTE_NORMAL,
                          FILE_SHARE_READ,
                          FILE_OPEN_IF,
                          FILE_SYNCHRONOUS_IO_NONALERT,
                          NULL,
                          0);

    if (NT_SUCCESS(status)) {
        data.OpenStatus = openStatus;
        data.MapStatus = mapStatus;
        
        ZwWriteFile(fileHandle,
                    NULL,
                    NULL,
                    NULL,
                    &ioStatus,
                    &data,
                    sizeof(data),
                    NULL,
                    NULL);

        ZwClose(fileHandle);
    }
}

/*-------------------------------------------------
   Server port callbacks (user-mode communication)
-------------------------------------------------*/
NTSTATUS
NovacachePortConnect(
    _In_ PFLT_PORT ClientPort,
    _In_ PVOID ServerPortCookie,
    _In_ PVOID ConnectionContext,
    _In_ ULONG SizeOfContext,
    _Out_ PVOID *ConnectionCookie
    )
{
    NTSTATUS status = STATUS_UNSUCCESSFUL;
    NTSTATUS openStatus = STATUS_UNSUCCESSFUL;
    NTSTATUS mapStatus = STATUS_UNSUCCESSFUL;

    UNREFERENCED_PARAMETER(ServerPortCookie);

    if (ConnectionContext == NULL || SizeOfContext != sizeof(NOVACACHE_CONNECTION_CONTEXT)) {
        *ConnectionCookie = NULL;
        FltCloseClientPort(gFilterHandle, &ClientPort);
        return STATUS_INVALID_PARAMETER;
    }

    PNOVACACHE_CONNECTION_CONTEXT ctx = (PNOVACACHE_CONNECTION_CONTEXT)ConnectionContext;
    SIZE_T viewSize = 0;

    status = ObReferenceObjectByHandle((HANDLE)ctx->SectionHandle,
                                       SECTION_MAP_READ | SECTION_MAP_WRITE,
                                       *MmSectionObjectType,
                                       UserMode,
                                       &gSharedMemSectionObject,
                                       NULL);
    openStatus = status;
    KdPrint(("Novacache: ObReferenceObjectByHandle Section status=0x%08x\n", status));

    if (!NT_SUCCESS(status)) {
        *ConnectionCookie = NULL;
        FltCloseClientPort(gFilterHandle, &ClientPort);
        return status;
    }

    PVOID mappedView = NULL;
    status = MmMapViewInSystemSpace(gSharedMemSectionObject,
                                    &mappedView,
                                    &viewSize);
    mapStatus = status;
    gSharedMemView = mappedView;
    mapStatus = status;
    KdPrint(("Novacache: MmMapViewInSystemSpace status=0x%08x, view=%p, size=%llu\n", status, gSharedMemView, (unsigned long long)viewSize));

    if (!NT_SUCCESS(status)) {
        ObDereferenceObject(gSharedMemSectionObject);
        gSharedMemSectionObject = NULL;
        *ConnectionCookie = NULL;
        FltCloseClientPort(gFilterHandle, &ClientPort);
        return status;
    }

    gSharedMemViewSize = viewSize;
    gSharedMemHeader = (PSHARED_MEM_HEADER)gSharedMemView;

    // Validate Capacity, RingCapacity and BlockSize from shared memory
    ULONG32 capacity = gSharedMemHeader->Capacity;
    ULONG32 ringCapacity = gSharedMemHeader->RingCapacity;
    ULONG32 blockSize = gSharedMemHeader->BlockSize;
    if (capacity == 0 || capacity > 1024 * 1024 || 
        ringCapacity == 0 || ringCapacity > 1024 * 1024 ||
        blockSize == 0 || blockSize > 1024 * 1024) 
    {
        KdPrint(("Novacache: Invalid Capacity %lu, RingCapacity %lu or BlockSize %lu, rejecting connection\n", capacity, ringCapacity, blockSize));
        MmUnmapViewInSystemSpace(gSharedMemView);
        gSharedMemView = NULL;
        ObDereferenceObject(gSharedMemSectionObject);
        gSharedMemSectionObject = NULL;
        gSharedMemHeader = NULL;
        *ConnectionCookie = NULL;
        FltCloseClientPort(gFilterHandle, &ClientPort);
        return STATUS_INVALID_PARAMETER;
    }

    // Validate that computed layout fits within the mapped view
    SIZE_T headerSize = sizeof(SHARED_MEM_HEADER);
    SIZE_T descArraySize = (SIZE_T)ringCapacity * sizeof(SHARED_MEM_BLOCK_DESC);
    SIZE_T ringDataSize = (SIZE_T)ringCapacity * CACHE_BLOCK_SIZE;
    SIZE_T dirArraySize = (SIZE_T)capacity * sizeof(CACHE_DIRECTORY_ENTRY);
    ULONG32 l2Capacity = gSharedMemHeader->L2Capacity;
    SIZE_T l2DirArraySize = (SIZE_T)l2Capacity * sizeof(CACHE_DIRECTORY_ENTRY);
    SIZE_T cacheDataSize = (SIZE_T)capacity * CACHE_BLOCK_SIZE;
    SIZE_T requiredSize = headerSize + descArraySize + ringDataSize + dirArraySize + l2DirArraySize + cacheDataSize;

    if (requiredSize > viewSize) {
        KdPrint(("Novacache: Shared memory too small: required=%llu, available=%llu\n",
                 (unsigned long long)requiredSize, (unsigned long long)viewSize));
        MmUnmapViewInSystemSpace(gSharedMemView);
        gSharedMemView = NULL;
        ObDereferenceObject(gSharedMemSectionObject);
        gSharedMemSectionObject = NULL;
        gSharedMemHeader = NULL;
        *ConnectionCookie = NULL;
        FltCloseClientPort(gFilterHandle, &ClientPort);
        return STATUS_BUFFER_TOO_SMALL;
    }

    gSharedMemDescriptors = (PSHARED_MEM_BLOCK_DESC)((PUCHAR)gSharedMemView + headerSize);
    gSharedMemData = (PUCHAR)gSharedMemDescriptors + descArraySize;
    gCacheDirectory = (PCACHE_DIRECTORY_ENTRY)(gSharedMemData + ringDataSize);
    gL2CacheDirectory = (PCACHE_DIRECTORY_ENTRY)((PUCHAR)gCacheDirectory + dirArraySize);
    gCacheData = (PUCHAR)gL2CacheDirectory + l2DirArraySize;

    // Use IoCreateNotificationEvent instead of passing handle, to prevent handle leaks
    ctx->EventName[63] = L'\0';
    UNICODE_STRING eventName;
    RtlInitUnicodeString(&eventName, ctx->EventName);
    HANDLE eventHandle;
    PKEVENT pEvent = IoCreateNotificationEvent(&eventName, &eventHandle);
    if (pEvent) {
        ObReferenceObject(pEvent);
        ZwClose(eventHandle);
        gSharedMemEvent = pEvent;
        status = STATUS_SUCCESS;
    } else {
        gSharedMemEvent = NULL;
        status = STATUS_UNSUCCESSFUL;
    }
    KdPrint(("Novacache: IoCreateNotificationEvent status=0x%08x, event=%p\n", status, gSharedMemEvent));
    if (!NT_SUCCESS(status)) {
        // Event is non-fatal; we can still operate without signaling user-mode
    }

    // Store performance counter frequency and reset counters BEFORE enabling accesses
    LARGE_INTEGER freq;
    KeQueryPerformanceCounter(&freq);
    gSharedMemHeader->PerfCounterFreq = freq.QuadPart;
    gSharedMemHeader->CachedHits = 0;
    gSharedMemHeader->CachedReadsTotal = 0;
    gSharedMemHeader->CachedWritesTotal = 0;
    gSharedMemHeader->DirtyCount = 0;

    // CRITICAL: Invalidate ALL cache directory entries from previous sessions.
    // Without this, stale Valid=1 entries cause the driver to serve wrong data.
    {
        ULONG32 cap = gSharedMemHeader->Capacity;
        if (cap > 0 && gCacheDirectory != NULL) {
            for (ULONG32 i = 0; i < cap; i++) {
                InterlockedExchange((LONG *)&gCacheDirectory[i].Valid, 0);
                gCacheDirectory[i].SequenceNum = 0;
            }
            // Memory barrier to ensure all invalidations are visible before enabling
            KeMemoryBarrier();
            KdPrint(("Novacache: Invalidated %lu cache directory entries on connect\n", cap));
        }

        ULONG32 l2Cap = gSharedMemHeader->L2Capacity;
        if (l2Cap > 0 && gL2CacheDirectory != NULL) {
            for (ULONG32 i = 0; i < l2Cap; i++) {
                InterlockedExchange((LONG *)&gL2CacheDirectory[i].Valid, 0);
                gL2CacheDirectory[i].SequenceNum = 0;
            }
            // Memory barrier to ensure all invalidations are visible before enabling
            KeMemoryBarrier();
            KdPrint(("Novacache: Invalidated %lu L2 cache directory entries on connect\n", l2Cap));
        }
    }

    // Open dynamic L2 path if provided
    if (ctx->L2Path[0] != L'\0') {
        UNICODE_STRING l2Name;
        OBJECT_ATTRIBUTES l2Attr;
        IO_STATUS_BLOCK l2Iosb;
        ctx->L2Path[259] = L'\0';
        RtlInitUnicodeString(&l2Name, ctx->L2Path);
        InitializeObjectAttributes(&l2Attr, &l2Name, OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE, NULL, NULL);
        
        NTSTATUS openStatusL2 = ZwCreateFile(&gL2CacheFileHandle,
                                             GENERIC_READ,
                                             &l2Attr,
                                             &l2Iosb,
                                             NULL,
                                             FILE_ATTRIBUTE_NORMAL,
                                             FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                             FILE_OPEN,
                                             FILE_SYNCHRONOUS_IO_NONALERT,
                                             NULL, 0);
        if (NT_SUCCESS(openStatusL2)) {
            ObReferenceObjectByHandle(gL2CacheFileHandle, FILE_READ_DATA, *IoFileObjectType, KernelMode, (PVOID*)&gL2CacheFileObject, NULL);
            KdPrint(("Novacache: Opened dynamic L2 Cache file successfully: %ws\n", ctx->L2Path));
        } else {
            gL2CacheFileHandle = NULL;
            gL2CacheFileObject = NULL;
            KdPrint(("Novacache: Failed to open dynamic L2 Cache file: %ws. Status: 0x%08X\n", ctx->L2Path, openStatusL2));
        }
    }

    // Enable accesses atomically — after all init is complete
    InterlockedExchange(&gSharedMemRefAndState, 0x80000000);

    WriteStatusToFile(openStatus, mapStatus);

    *ConnectionCookie = ClientPort;
    return STATUS_SUCCESS;
}

VOID
NovacachePortDisconnect(
    _In_ PVOID ConnectionCookie
    )
{
    if (ConnectionCookie != NULL) {
        PFLT_PORT clientPort = (PFLT_PORT)ConnectionCookie;
        FltCloseClientPort(gFilterHandle, &clientPort);
    }

    // 1. Clear Active flag atomically
    LONG oldVal, newVal;
    do {
        oldVal = gSharedMemRefAndState;
        newVal = oldVal & 0x7FFFFFFF;
    } while (InterlockedCompareExchange(&gSharedMemRefAndState, newVal, oldVal) != oldVal);

    // 2. Wait until reference count drops to 0 (with timeout)
    LARGE_INTEGER timeout;
    timeout.QuadPart = -50000000LL; // 5 second timeout (100ns units, negative = relative)
    LARGE_INTEGER start;
    KeQuerySystemTimePrecise(&start);

    while ((gSharedMemRefAndState & 0x7FFFFFFF) > 0) {
        LARGE_INTEGER now;
        KeQuerySystemTimePrecise(&now);
        if (now.QuadPart - start.QuadPart > 50000000LL) { // 5 seconds
            KdPrint(("Novacache: WARNING: Timed out waiting for refs to drain during disconnect\n"));
            break;
        }
        LARGE_INTEGER interval;
        interval.QuadPart = -10000; // 1 ms
        KeDelayExecutionThread(KernelMode, FALSE, &interval);
    }

    // 3. Cleanup pointers first (before unmapping) to prevent concurrent dereference of unmapped memory
    gSharedMemHeader = NULL;
    gSharedMemDescriptors = NULL;
    gSharedMemData = NULL;
    gCacheDirectory = NULL;
    gL2CacheDirectory = NULL;
    gCacheData = NULL;

    KeMemoryBarrier();

    if (gSharedMemEvent) {
        ObDereferenceObject(gSharedMemEvent);
        gSharedMemEvent = NULL;
    }

    if (gSharedMemSectionObject) {
        ObDereferenceObject(gSharedMemSectionObject);
        gSharedMemSectionObject = NULL;
    }

    if (gSharedMemView) {
        MmUnmapViewInSystemSpace(gSharedMemView);
        gSharedMemView = NULL;
    }

    if (gL2CacheFileObject) {
        ObDereferenceObject(gL2CacheFileObject);
        gL2CacheFileObject = NULL;
    }
    if (gL2CacheFileHandle) {
        ZwClose(gL2CacheFileHandle);
        gL2CacheFileHandle = NULL;
    }
}

NTSTATUS
NovacachePortMessage(
    _In_ PVOID ConnectionCookie,
    _In_reads_bytes_(InputBufferLength) PVOID InputBuffer,
    _In_ ULONG InputBufferLength,
    _Out_writes_bytes_to_(OutputBufferLength,*ReturnOutputBufferLength) PVOID OutputBuffer,
    _In_ ULONG OutputBufferLength,
    _Out_ PULONG ReturnOutputBufferLength
    )
{
    UNREFERENCED_PARAMETER(ConnectionCookie);

    if (OutputBufferLength > 0 && OutputBuffer == NULL) {
        return STATUS_INVALID_PARAMETER;
    }

    // Command request from user-mode
    typedef struct {
        ULONG32 Command;
        ULONG32 CacheSizeMB;
        ULONG32 BlockSizeKB;
        WCHAR VolumeGuid[128];
    } NOVACACHE_COMMAND_REQUEST, *PNOVACACHE_COMMAND_REQUEST;

    if (InputBuffer != NULL && InputBufferLength >= sizeof(NOVACACHE_COMMAND_REQUEST)) {
        PNOVACACHE_COMMAND_REQUEST cmd = (PNOVACACHE_COMMAND_REQUEST)InputBuffer;

        KdPrint(("Novacache: PortMessage command=%lu cacheSize=%luKB block=%luKB\n",
                 cmd->Command, cmd->CacheSizeMB, cmd->BlockSizeKB));

        switch (cmd->Command) {
            case 1: // StartCaching
                {
                    ULONG32 bitmap = 0;
                    if (AcquireSharedMemReference()) {
                        if (gSharedMemHeader) {
                            bitmap = gSharedMemHeader->VolumeBitmap;
                        }
                        ReleaseSharedMemReference();
                    }
                    KdPrint(("Novacache: StartCaching received. Bitmap=0x%08x\n", bitmap));
                }
                break;
            case 2: // StopCaching
                KdPrint(("Novacache: StopCaching received\n"));
                break;
            default:
                KdPrint(("Novacache: Unknown command %lu\n", cmd->Command));
                break;
        }
    }

    if (OutputBufferLength < sizeof(NOVACACHE_STATS))
        return STATUS_BUFFER_TOO_SMALL;

    RtlZeroMemory(OutputBuffer, sizeof(NOVACACHE_STATS));

    {
        PFLT_INSTANCE *instances = NULL;
        ULONG count = 0;
        
        NTSTATUS st = FltEnumerateInstances(NULL, gFilterHandle, NULL, 0, &count);
        if (st == STATUS_BUFFER_TOO_SMALL && count > 0) {
            instances = ExAllocatePool2(POOL_FLAG_PAGED,
                                        count * sizeof(PFLT_INSTANCE),
                                        NOVACACHE_TAG);
            if (instances) {
                st = FltEnumerateInstances(NULL, gFilterHandle, instances, count, &count);
                if (NT_SUCCESS(st)) {
                    for (ULONG i = 0; i < count; ++i) {
                        PNOVACACHE_INSTANCE_CONTEXT ic = NULL;
                        if (NT_SUCCESS(FltGetInstanceContext(instances[i],
                                                             (PFLT_CONTEXT *)&ic)) && ic) {
                            ((PNOVACACHE_STATS)OutputBuffer)->ReadOperations  += ic->ReadOps;
                            ((PNOVACACHE_STATS)OutputBuffer)->WriteOperations += ic->WriteOps;
                            FltReleaseContext(ic);
                        }
                        FltObjectDereference(instances[i]);
                    }
                }
                ExFreePoolWithTag(instances, NOVACACHE_TAG);
            }
        }
    }

    *ReturnOutputBufferLength = sizeof(NOVACACHE_STATS);
    return STATUS_SUCCESS;
}

/*-------------------------------------------------
   Pre-operation callback for IRP_MJ_WRITE
   Lets write proceed to disk in all modes.
   PostOp handles write-allocate + ring push with
   appropriate flag (WRITE_BACK or WRITE_THROUGH).
-------------------------------------------------*/
FLT_PREOP_CALLBACK_STATUS
NovacachePreOperationWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_    PCFLT_RELATED_OBJECTS FltObjects,
    _Out_   PVOID *CompletionContext
    )
{
    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS status;

    status = FltGetInstanceContext(FltObjects->Instance,
                                   (PFLT_CONTEXT *)&ctx);
    if (!NT_SUCCESS(status) || ctx == NULL) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    if (!NovacacheIsCacheableFileObject(FltObjects)) {
        InterlockedIncrement64(&ctx->WriteOps);
        FltReleaseContext(ctx);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    // For paging writes, we must invalidate the cache entries in the written range to prevent cache incoherency!
    if (FlagOn(Data->Iopb->IrpFlags, IRP_PAGING_IO)) {
        InterlockedIncrement64(&ctx->WriteOps);
        ULONG32 volId = ctx->VolumeId;
        ULONG64 offset = Data->Iopb->Parameters.Write.ByteOffset.QuadPart;
        ULONG length = Data->Iopb->Parameters.Write.Length;

        if (length > 0 && AcquireSharedMemReference()) {
            ULONG32 capacity = gSharedMemHeader->Capacity;
            PCACHE_DIRECTORY_ENTRY dir = gCacheDirectory;
            if (capacity > 0 && dir != NULL) {
                ULONG32 numBuckets = capacity / 2;
                ULONG64 startChunk = offset / CACHE_BLOCK_SIZE;
                ULONG64 endChunk = (offset + length - 1) / CACHE_BLOCK_SIZE;
                ULONG numChunks = (ULONG)(endChunk - startChunk + 1);

                ULONG64 fileId = 0;
                // QueryIfMissing = FALSE to avoid deadlocks at paging IRQL
                if (NT_SUCCESS(NovacacheGetFileId(FltObjects->Instance, FltObjects->FileObject, FALSE, &fileId))) {
                    PVOID fileObject = (PVOID)fileId;

                    if (numBuckets > 0) {
                        for (ULONG i = 0; i < numChunks; i++) {
                            ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                            ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                            for (ULONG w = 0; w < 2; w++) {
                                ULONG32 si = bucket * 2 + w;
                                if (si < capacity && dir[si].Valid == 1 &&
                                    dir[si].VolumeId == volId &&
                                    dir[si].Offset == chunkOffset &&
                                    dir[si].FileObject == fileObject)
                                {
                                    InterlockedExchange((LONG *)&dir[si].Valid, 0);
                                }
                            }
                        }
                    }

                    // Also invalidate dynamic L2 cache entries matching this chunk range
                    ULONG32 l2Capacity = gSharedMemHeader->L2Capacity;
                    PCACHE_DIRECTORY_ENTRY l2Dir = gL2CacheDirectory;
                    if (l2Capacity > 0 && l2Dir != NULL) {
                        ULONG32 numL2Buckets = l2Capacity / 4;
                        if (numL2Buckets > 0) {
                            for (ULONG i = 0; i < numChunks; i++) {
                                ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                                ULONG32 l2Bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numL2Buckets);
                                for (ULONG j = 0; j < 4; j++) {
                                    ULONG32 s = l2Bucket * 4 + j;
                                    if (s < l2Capacity && l2Dir[s].Valid == 1 &&
                                        l2Dir[s].VolumeId == volId &&
                                        l2Dir[s].Offset == chunkOffset &&
                                        l2Dir[s].FileObject == fileObject)
                                    {
                                        InterlockedExchange((LONG *)&l2Dir[s].Valid, 0);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ReleaseSharedMemReference();
        }

        FltReleaseContext(ctx);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    ULONG32 volId = ctx->VolumeId;

    // Skip caching for disabled volumes
    if (!IsVolumeEnabled(volId)) {
        InterlockedIncrement64(&ctx->WriteOps);
        FltReleaseContext(ctx);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    // Lock user buffer if it's user-mode and we don't have MDL yet, so it is safe in PostOp
    ULONG length = Data->Iopb->Parameters.Write.Length;
    if (length > 0 && FLT_IS_IRP_OPERATION(Data)) {
        if (Data->Iopb->Parameters.Write.MdlAddress == NULL) {
            PVOID rawBuffer = Data->Iopb->Parameters.Write.WriteBuffer;
            if (rawBuffer != NULL && (ULONG_PTR)rawBuffer < MmUserProbeAddress) {
                __try {
                    FltLockUserBuffer(Data);
                } __except (EXCEPTION_EXECUTE_HANDLER) {
                    KdPrint(("Novacache: Exception in FltLockUserBuffer (Write): 0x%08x\n", GetExceptionCode()));
                }
            }
        }
    }

    InterlockedIncrement64(&ctx->WriteOps);
    FltReleaseContext(ctx);

    LARGE_INTEGER perfFreq;
    LARGE_INTEGER now = KeQueryPerformanceCounter(&perfFreq);

    *CompletionContext = (PVOID)now.QuadPart;
    return FLT_PREOP_SUCCESS_WITH_CALLBACK;
}

/*-------------------------------------------------
   Post-operation callback for IRP_MJ_WRITE
   After disk write succeeds, write-allocate to L1 cache
   and push to ring. Uses WRITE_BACK flag when write-back
   mode is enabled (dirty tracking), WRITE_THROUGH otherwise.
-------------------------------------------------*/
FLT_POSTOP_CALLBACK_STATUS
NovacachePostOperationWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags
    )
{
    if (Flags & FLTFL_POST_OPERATION_DRAINING) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NT_SUCCESS(Data->IoStatus.Status) || Data->IoStatus.Information == 0) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (!NovacacheIsCacheableFileObject(FltObjects)) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    if (Data->Iopb->MajorFunction == IRP_MJ_WRITE) {
        if (!FLT_IS_IRP_OPERATION(Data)) {
            return NovacacheSafePostOperationWrite(Data, FltObjects, CompletionContext, Flags);
        }

        if (KeGetCurrentIrql() == DISPATCH_LEVEL) {
            FLT_POSTOP_CALLBACK_STATUS retStatus;
            if (FltDoCompletionProcessingWhenSafe(
                    Data,
                    FltObjects,
                    CompletionContext,
                    Flags,
                    NovacacheSafePostOperationWrite,
                    &retStatus
                ))
            {
                return retStatus;
            }
            return FLT_POSTOP_FINISHED_PROCESSING;
        } else {
            return NovacacheSafePostOperationWrite(Data, FltObjects, CompletionContext, Flags);
        }
    }

    return FLT_POSTOP_FINISHED_PROCESSING;
}

FLT_POSTOP_CALLBACK_STATUS
NovacacheSafePostOperationWrite(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags
    )
{
    UNREFERENCED_PARAMETER(FltObjects);
    UNREFERENCED_PARAMETER(Flags);

    if (!NT_SUCCESS(Data->IoStatus.Status) || Data->IoStatus.Information == 0) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    PVOID fileObject = NULL;
    ULONG64 fileId = 0;
    if (Data->Iopb->TargetFileObject == NULL) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    if (!NT_SUCCESS(NovacacheGetFileId(Data->Iopb->TargetInstance, Data->Iopb->TargetFileObject, TRUE, &fileId))) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    fileObject = (PVOID)fileId;

    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS ctxStatus = FltGetInstanceContext(Data->Iopb->TargetInstance, (PFLT_CONTEXT *)&ctx);
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
        BOOLEAN writeBackEnabled = (gSharedMemHeader->WriteBackEnabled != 0);

        if (Data->Iopb->Parameters.Write.MdlAddress != NULL) {
            buffer = MmGetSystemAddressForMdlSafe(Data->Iopb->Parameters.Write.MdlAddress, NormalPagePriority);
        } else {
            PVOID rawBuffer = Data->Iopb->Parameters.Write.WriteBuffer;
            if (rawBuffer != NULL && (ULONG_PTR)rawBuffer >= MmUserProbeAddress) {
                buffer = rawBuffer;
            }
        }

        if (length > 0) {
            ULONG64 offset = Data->Iopb->Parameters.Write.ByteOffset.QuadPart;
            ULONG64 startChunk = offset / CACHE_BLOCK_SIZE;
            ULONG64 endChunk = (offset + length - 1) / CACHE_BLOCK_SIZE;
            ULONG numChunks = (ULONG)(endChunk - startChunk + 1);
            UCHAR dummyData = 0;

            for (ULONG i = 0; i < numChunks; i++) {
                ULONG64 chunkOffset = (startChunk + i) * CACHE_BLOCK_SIZE;
                
                ULONG64 chunkStartInWrite = 0;
                if (chunkOffset > offset) {
                    chunkStartInWrite = chunkOffset - offset;
                }
                
                ULONG64 chunkDataOffset = 0;
                if (offset > chunkOffset) {
                    chunkDataOffset = offset - chunkOffset;
                }
                
                ULONG64 copyLen = CACHE_BLOCK_SIZE - chunkDataOffset;
                if (chunkStartInWrite + copyLen > length) {
                    copyLen = length - chunkStartInWrite;
                }

                if (copyLen > 0) {
                    // Update L1 cache in shared memory if chunk is block-aligned AND we have valid buffer
                    if (buffer != NULL && copyLen == CACHE_BLOCK_SIZE && chunkDataOffset == 0) {
                        PCACHE_DIRECTORY_ENTRY dir = gCacheDirectory;
                        PUCHAR cacheData = gCacheData;
                        ULONG32 capacity = gSharedMemHeader->Capacity;
                        if (dir && cacheData && capacity > 0) {
                            ULONG32 numBuckets = capacity / 2;
                            if (numBuckets > 0) {
                                ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                ULONG32 targetSlot = bucket * 2;

                                for (ULONG32 w = 0; w < 2; w++) {
                                    ULONG32 si = bucket * 2 + w;
                                    if (si < capacity) {
                                        if (dir[si].Valid == 1 &&
                                            dir[si].VolumeId == volId &&
                                            dir[si].Offset == chunkOffset &&
                                            dir[si].FileObject == fileObject)
                                        {
                                            targetSlot = si;
                                            break;
                                        }
                                        if (dir[si].Valid == 0) {
                                            targetSlot = si;
                                        }
                                    }
                                }

                                PUCHAR slotPtr = cacheData + ((ULONG64)targetSlot * CACHE_BLOCK_SIZE);

                                __try {
                                    RtlCopyMemory(slotPtr, (PUCHAR)buffer + chunkStartInWrite, (SIZE_T)copyLen);
                                    KeMemoryBarrier();
                                    InterlockedExchange((LONG *)&dir[targetSlot].Valid, 0);
                                    dir[targetSlot].Offset = chunkOffset;
                                    dir[targetSlot].VolumeId = volId;
                                    dir[targetSlot].SlotIndex = targetSlot;
                                    dir[targetSlot].Length = (ULONG32)copyLen;
                                    dir[targetSlot].FileObject = fileObject;
                                    InterlockedIncrement64((volatile LONG64 *)&dir[targetSlot].SequenceNum);
                                    KeMemoryBarrier();
                                    InterlockedExchange((LONG *)&dir[targetSlot].Valid, 1);
                                } __except (EXCEPTION_EXECUTE_HANDLER) {
                                    InterlockedExchange((LONG *)&dir[targetSlot].Valid, 0);
                                }
                            }
                        }
                    } else {
                        // Sub-block write OR null buffer: invalidate L1 slot if it exists
                        PCACHE_DIRECTORY_ENTRY dir = gCacheDirectory;
                        ULONG32 capacity = gSharedMemHeader->Capacity;
                        if (dir && capacity > 0) {
                            ULONG32 numBuckets = capacity / 2;
                            if (numBuckets > 0) {
                                ULONG32 bucket = (ULONG32)((chunkOffset / CACHE_BLOCK_SIZE) % numBuckets);
                                for (ULONG32 w = 0; w < 2; w++) {
                                    ULONG32 si = bucket * 2 + w;
                                    if (si < capacity && dir[si].Valid == 1 &&
                                        dir[si].VolumeId == volId &&
                                        dir[si].Offset == chunkOffset &&
                                        dir[si].FileObject == fileObject)
                                    {
                                        InterlockedExchange((LONG *)&dir[si].Valid, 0);
                                    }
                                }
                            }
                        }
                    }

                    // Push to shared memory ring buffer
                    {
                        LARGE_INTEGER perfFreq;
                        LARGE_INTEGER postOpTick = KeQueryPerformanceCounter(&perfFreq);

                        ULONG32 ringFlags = writeBackEnabled ?
                            SHARED_MEM_FLAG_WRITE_BACK : SHARED_MEM_FLAG_WRITE_THROUGH;

                        __try {
                            if (buffer != NULL) {
                                NovacacheWriteToSharedRing(volId, chunkOffset, (ULONG32)copyLen, ringFlags,
                                                           (ULONG64)CompletionContext, postOpTick.QuadPart,
                                                           fileObject, (PUCHAR)buffer + chunkStartInWrite);
                            } else {
                                // If buffer is NULL, send a 1-byte dummy write to force the Rust service to invalidate L2
                                NovacacheWriteToSharedRing(volId, chunkOffset, 1, ringFlags,
                                                           (ULONG64)CompletionContext, postOpTick.QuadPart,
                                                           fileObject, &dummyData);
                            }
                            InterlockedIncrement64(&gSharedMemHeader->CachedWritesTotal);
                            if (writeBackEnabled) {
                                InterlockedIncrement((volatile LONG *)&gSharedMemHeader->DirtyCount);
                            }
                        } __except (EXCEPTION_EXECUTE_HANDLER) {
                        }
                    }
                }
            }
        }
        ReleaseSharedMemReference();
    }

    return FLT_POSTOP_FINISHED_PROCESSING;
}

/*-------------------------------------------------
   Pre-operation callback for IRP_MJ_ACQUIRE_FOR_SECTION_SYNCHRONIZATION
   Intercepts memory-mapped I/O: when a file section is about to be
   synchronized, we treat the underlying read as a cacheable event.
   For non-cached reads that back mmap, we push data to the ring.
-------------------------------------------------*/
FLT_PREOP_CALLBACK_STATUS
NovacachePreOperationSectionSync(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_    PCFLT_RELATED_OBJECTS FltObjects,
    _Out_   PVOID *CompletionContext
    )
{
    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(CompletionContext);

    status = FltGetInstanceContext(FltObjects->Instance,
                                   (PFLT_CONTEXT *)&ctx);
    if (!NT_SUCCESS(status) || ctx == NULL) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    InterlockedIncrement64(&ctx->ReadOps);
    FltReleaseContext(ctx);

    // Let the section sync proceed — the actual page-in will flow through
    // normal IRP_MJ_READ / IRP_MJ_MDL_READ paths that we already cache.
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
}

/*-------------------------------------------------
   Pre-operation callback for IRP_MJ_SET_INFORMATION
   Handles cache invalidation when file metadata changes:
   - FileEndOfFileInformation (truncate): invalidate blocks beyond new size
   - FileDispositionInformation (delete): invalidate all blocks for the file
   - FileRenameInformation: invalidate all blocks for old name
-------------------------------------------------*/
FLT_PREOP_CALLBACK_STATUS
NovacachePreOperationSetInfo(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_    PCFLT_RELATED_OBJECTS FltObjects,
    _Out_   PVOID *CompletionContext
    )
{
    PNOVACACHE_INSTANCE_CONTEXT ctx = NULL;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(CompletionContext);

    status = FltGetInstanceContext(FltObjects->Instance,
                                   (PFLT_CONTEXT *)&ctx);
    if (!NT_SUCCESS(status) || ctx == NULL) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    FILE_INFORMATION_CLASS infoClass = Data->Iopb->Parameters.SetFileInformation.FileInformationClass;

    if (infoClass == FileEndOfFileInformation ||
        infoClass == FileDispositionInformation ||
        infoClass == FileDispositionInformationEx ||
        infoClass == FileRenameInformation)
    {
        // For file size changes, deletions, and renames:
        // Invalidate cache directory entries for this file by clearing matching buckets.
        // This is a simple but effective approach — the cache directory is direct-mapped.
        if (AcquireSharedMemReference()) {
            ULONG32 capacity = gSharedMemHeader->Capacity;

            if (capacity > 0 && gCacheDirectory != NULL) {
                // Invalidate cache entries for this specific file (FileObject + VolumeId)
                ULONG32 volId = ctx->VolumeId;
                PVOID fileObject = NULL;
                ULONG64 fileId = 0;
                if (NT_SUCCESS(NovacacheGetFileId(FltObjects->Instance, FltObjects->FileObject, TRUE, &fileId))) {
                    fileObject = (PVOID)fileId;

                    if (infoClass == FileDispositionInformation || infoClass == FileDispositionInformationEx) {
                        PNOVACACHE_STREAM_CONTEXT streamCtx = NULL;
                        if (NT_SUCCESS(FltGetStreamContext(FltObjects->Instance, FltObjects->FileObject, (PFLT_CONTEXT *)&streamCtx)) && streamCtx != NULL) {
                            streamCtx->DeleteOnClose = TRUE;
                            FltReleaseContext(streamCtx);
                        }
                    }
                    ULONG32 invalidated = 0;

                    for (ULONG32 i = 0; i < capacity && invalidated < 256; i++) {
                        if (gCacheDirectory[i].Valid == 1 &&
                            gCacheDirectory[i].VolumeId == volId &&
                            gCacheDirectory[i].FileObject == fileObject)
                        {
                            // Mark invalid by clearing the Valid flag
                            InterlockedExchange((LONG *)&gCacheDirectory[i].Valid, 0);
                            invalidated++;
                        }
                    }

                    // Also invalidate dynamic L2 cache entries matching this FileObject
                    ULONG32 l2Capacity = gSharedMemHeader->L2Capacity;
                    PCACHE_DIRECTORY_ENTRY l2Dir = gL2CacheDirectory;
                    if (l2Capacity > 0 && l2Dir != NULL) {
                        for (ULONG32 i = 0; i < l2Capacity; i++) {
                            if (l2Dir[i].Valid == 1 &&
                                l2Dir[i].VolumeId == volId &&
                                l2Dir[i].FileObject == fileObject)
                            {
                                InterlockedExchange((LONG *)&l2Dir[i].Valid, 0);
                                invalidated++;
                            }
                        }
                    }

                    // Send invalidation request to user-mode service
                    UCHAR dummyData = 0;
                    NovacacheWriteToSharedRing(volId, 0, 0, SHARED_MEM_FLAG_INVALIDATE, 0, 0, fileObject, &dummyData);

                    KdPrint(("Novacache: SET_INFO invalidation volId=%u class=%d total_invalidated=%lu\n",
                             volId, infoClass, invalidated));
                }
            }
            ReleaseSharedMemReference();
        }
    }

    FltReleaseContext(ctx);

    // Always let the operation proceed
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
}

/*-------------------------------------------------
   Write to shared memory ring buffer
-------------------------------------------------*/
VOID
NovacacheWriteToSharedRing(
    _In_ ULONG32 VolumeId,
    _In_ ULONG64 Offset,
    _In_ ULONG32 Length,
    _In_ ULONG32 Flags,
    _In_ ULONG64 PreOpTick,
    _In_ ULONG64 PostOpTick,
    _In_ PVOID FileObject,
    _In_reads_bytes_(Length) PVOID Data
    )
{
    PSHARED_MEM_HEADER hdr = gSharedMemHeader;
    PSHARED_MEM_BLOCK_DESC descs = gSharedMemDescriptors;
    PUCHAR ringData = gSharedMemData;

    if (!hdr || !descs || !ringData || (Length == 0 && !(Flags & SHARED_MEM_FLAG_INVALIDATE)) || Length > CACHE_BLOCK_SIZE || FileObject == NULL) {
        return;
    }

    ULONG32 ringCapacity = hdr->RingCapacity;
    if (ringCapacity == 0) {
        return;
    }

    KeEnterCriticalRegion();
    ExAcquirePushLockExclusive(&gRingBufferLock);

    __try {
        ULONG64 head = hdr->Head;
        ULONG64 tail = hdr->Tail;

        if (head - tail >= ringCapacity) {
            ExReleasePushLockExclusive(&gRingBufferLock);
            KeLeaveCriticalRegion();
            return;
        }

        ULONG32 idx = (ULONG32)(head % ringCapacity);

        descs[idx].VolumeId = VolumeId;
        descs[idx].Offset = Offset;
        descs[idx].Length = Length;
        descs[idx].Flags = Flags;
        descs[idx].PreOpTick = PreOpTick;
        descs[idx].PostOpTick = PostOpTick;
        descs[idx].FileObject = (ULONG64)FileObject;
        descs[idx].Crc32 = 0;

        PUCHAR dest = ringData + ((ULONG64)idx * CACHE_BLOCK_SIZE);
        RtlCopyMemory(dest, Data, Length);

        InterlockedExchange64((volatile LONG64*)&hdr->Head, head + 1);
        KeMemoryBarrier();
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        KdPrint(("Novacache: Exception in WriteToSharedRing: 0x%08x\n", GetExceptionCode()));
    }

    ExReleasePushLockExclusive(&gRingBufferLock);
    KeLeaveCriticalRegion();

    if (gSharedMemEvent) {
        KeSetEvent(gSharedMemEvent, 0, FALSE);
    }
}
