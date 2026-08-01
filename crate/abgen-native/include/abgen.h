/*
 * abgen — Decentraland asset-bundle conversion as a C-ABI shared library.
 * Link against libabgen.so / libabgen.dylib / abgen.dll, or libabgen.a.
 *
 *   if (abgen_abi_version() != ABGEN_ABI_VERSION) return -1;
 *   abgen_set_max_threads(4);
 *   int32_t rc = abgen_convert(req, req_len, on_emit, &state);
 *
 * MEMORY. Payload pointers are borrowed for the callback only; check `len`
 * before dereferencing. The request buffer stays the caller's.
 *
 * THREADS. Synchronous, calls back on the calling thread, blocking and
 * CPU-heavy. Not reentrant: never call it from inside its own callback.
 *
 * FAILURE. No panic escapes; unwinds surface as ABGEN_ERR_PANIC. A bad asset
 * is not a bad run — abgen_convert() returns ABGEN_OK even when every model
 * failed; per-file outcomes are in the events and the manifest.
 *
 * The exception is the library being unable to obtain its own build templates,
 * which is a bad run and returns ABGEN_ERR_CONVERT_FAILED with an error event.
 * That cannot happen by accident: the templates are compiled into this library,
 * so it needs no data files beside it and nothing to configure. It happens only
 * if the host sets ABGEN_ROOT to a directory it cannot read, and then failing
 * is the point — silently falling back to the built-in copy would hide the
 * typo. Earlier versions resolved templates from disk and returned ABGEN_OK
 * with zero bundles when they were absent, which looked like success.
 */

#ifndef ABGEN_H
#define ABGEN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Compare against abgen_abi_version(); refuse to load on a mismatch. */
#define ABGEN_ABI_VERSION 1u

typedef enum {
    /* UTF-8 JSON progress event, discriminated by its "ev" field. */
    ABGEN_KIND_JSON = 0,
    /* One artifact: uint32 name_len | name | uint32 data_len | data. */
    ABGEN_KIND_OUTPUT = 1,
    /* UTF-8 error message, fatal to the run. */
    ABGEN_KIND_ERROR = 2,
    /* UTF-8 JSON job manifest. Once, last, convert mode only. */
    ABGEN_KIND_MANIFEST = 3
} AbgenKind;

/* Return codes. */
#define ABGEN_OK                     0
#define ABGEN_ERR_MALFORMED_INPUT    1  /* request did not parse */
#define ABGEN_ERR_CONVERT_FAILED     2  /* conversion failed      */
#define ABGEN_ERR_NULL_ARG           3  /* null/empty argument    */
#define ABGEN_ERR_PANIC              4  /* unwind caught at the boundary */
#define ABGEN_ERR_ALREADY_CONFIGURED 5  /* thread pool already built */

/* Conversion modes, encoded in the request blob. */
#define ABGEN_MODE_CONVERT      0  /* convert every model, bake LOD, manifest */
#define ABGEN_MODE_SCAN         1  /* report plan + deps, convert nothing     */
#define ABGEN_MODE_CONVERT_ONLY 2  /* convert one named model                 */
#define ABGEN_MODE_LOD_ONLY     3  /* bake the scene LOD only                 */

typedef void (*AbgenEmitFn)(void *user_data, uint32_t kind,
                            const uint8_t *ptr, size_t len);

uint32_t abgen_abi_version(void);

/* Static NUL-terminated version string; do not free. */
const char *abgen_version(void);

/* Caps the CPU-bound worker pool, which otherwise sizes to every core and
 * competes with the host's own threads. Process-wide, effective once. */
int32_t abgen_set_max_threads(uint32_t threads);

/* Only needed if abgen should own the request buffer. abgen_alloc(0) is NULL,
 * abgen_free(NULL, ..) a no-op, and `len` must match the allocation. */
uint8_t *abgen_alloc(size_t len);
void abgen_free(uint8_t *ptr, size_t len);

/* Converts one request; results arrive through `emit` before this returns.
 *
 * Blob layout (all integers little-endian uint32_t):
 *
 *   uint32 file_count
 *     repeated file_count times:
 *       uint32 name_len, name bytes (utf-8 path)
 *       uint32 data_len, data bytes
 *   uint32 len, bytes   platform      "windows" | "mac" | "linux" | "webgl"
 *   uint32 len, bytes   entity_type   "" to detect from the files
 *   uint8  magenta_missing
 *   uint8  bake_lod
 *   uint8  mode                       ABGEN_MODE_*
 *   uint8  crop
 *   uint32 tri_cap                    0 = uncapped
 *   uint32 len, bytes   entity_hash   "" to derive; names the LOD (mode 3)
 *   uint32 len, bytes   only_glb      mode 2: which file to convert
 *   uint32 entry_count                optional content table; 0 to derive
 *     repeated entry_count times:
 *       uint32 len, bytes  file name
 *       uint32 len, bytes  content hash
 *
 * Everything from tri_cap on is optional: a request that stops early takes
 * defaults for the rest. */
int32_t abgen_convert(const uint8_t *request, size_t request_len,
                      AbgenEmitFn emit, void *user_data);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ABGEN_H */
