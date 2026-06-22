/*=====================================================================
  Novacache.h  –  Shared definitions between driver and user-mode
=====================================================================*/

#ifndef _NOVACACHE_H_
#define _NOVACACHE_H_

#include <ntifs.h>
#include <fltKernel.h>
#include <ntstrsafe.h>

#define NOVACACHE_PORT_NAME      L"\\NovaCachePort"
#define NOVACACHE_DEVICE_NAME    L"\\Device\\Novacache"
#define NOVACACHE_SYMBOLIC_NAME  L"\\DosDevices\\Novacache"

/*=== IOCTLs that user–mode may send ================================*/
#define IOCTL_NOVACACHE_GET_STATS CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)

/*=== Simple statistics structure ====================================*/
typedef struct _NOVACACHE_STATS
{
    ULONG64 ReadOperations;
    ULONG64 WriteOperations;
} NOVACACHE_STATS, *PNOVACACHE_STATS;

/*=== Connection Context =============================================*/
#pragma pack(push, 1)
typedef struct _NOVACACHE_CONNECTION_CONTEXT {
    ULONG64 SectionHandle;
    WCHAR EventName[64];
    WCHAR L2Path[260];
} NOVACACHE_CONNECTION_CONTEXT, *PNOVACACHE_CONNECTION_CONTEXT;
#pragma pack(pop)

/*=== Shared Memory Ring Buffer Definitions ===========================*/

typedef struct _SHARED_MEM_BLOCK_DESC {
    ULONG64 SequenceNum;
    ULONG32 VolumeId;
    ULONG32 Flags;
    ULONG64 Offset;
    ULONG32 Length;
    ULONG32 Status;
    ULONG64 PreOpTick;     /* KeQueryPerformanceCounter at IRP arrival */
    ULONG64 PostOpTick;    /* KeQueryPerformanceCounter at IRP completion */
    ULONG64 FileObject;    /* FileObject pointer for L1 cache key matching */
    ULONG32 Crc32;         /* CRC32 of block data (computed by service) */
    ULONG32 Padding;       /* alignment */
} SHARED_MEM_BLOCK_DESC, *PSHARED_MEM_BLOCK_DESC;

/* Shared memory block descriptor flags */
#define SHARED_MEM_FLAG_WRITE_THROUGH  0x00000001  /* Block is from a write operation */
#define SHARED_MEM_FLAG_WRITE_BACK     0x00000002  /* Block is write-back (dirty, not on disk yet) */
#define SHARED_MEM_FLAG_DIRTY          0x00000004  /* Block data is dirty (needs flush) */
#define SHARED_MEM_FLAG_INVALIDATE     0x00000008  /* File is truncated/deleted/renamed (invalidate L1/L2) */

typedef struct _SHARED_MEM_HEADER {
    volatile ULONG64 Head;
    volatile ULONG64 Tail;
    ULONG32 Capacity;       /* cache_capacity — number of cache directory slots */
    ULONG32 BlockSize;
    volatile ULONG32 VolumeBitmap;
    ULONG32 RingCapacity;   /* ring_capacity — number of ring buffer slots */
    ULONG64 PerfCounterFreq; /* KeQueryPerformanceCounter frequency (ticks/sec) */
    volatile ULONG64 CachedHits;  /* driver-side cache hit counter */
    volatile ULONG64 CachedReadsTotal; /* driver-side total read counter */
    volatile ULONG64 CachedWritesTotal; /* driver-side total write counter */
    volatile ULONG32 WriteBackEnabled;  /* 0=write-through, 1=write-back */
    volatile ULONG32 DirtyCount;        /* number of dirty blocks pending flush */
    ULONG32 L2Capacity;                 /* number of slots in L2CacheDirectory */
    ULONG32 Reserved[1];
} SHARED_MEM_HEADER, *PSHARED_MEM_HEADER;

typedef struct _CACHE_DIRECTORY_ENTRY {
    ULONG64 Offset;
    ULONG32 VolumeId;
    ULONG32 SlotIndex;
    volatile ULONG32 Valid;
    ULONG32 Length;
    volatile ULONG64 SequenceNum;  /* generation counter — incremented on every update */
    PVOID FileObject;              /* FileObject pointer — disambiguates same-offset writes from different files */
} CACHE_DIRECTORY_ENTRY, *PCACHE_DIRECTORY_ENTRY;

/*=== Helper macro to get the filter’s instance context ===============*/
#define GET_INSTANCE_CONTEXT(Instance) \
    (PNOVACACHE_INSTANCE_CONTEXT)FltGetInstanceContext(Instance)

#endif // _NOVACACHE_H_
