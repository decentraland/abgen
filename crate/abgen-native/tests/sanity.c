/*
 * Proves the C ABI from C: the Rust tests call the same entry points as Rust
 * functions and cannot catch a broken export table or a drifted header.
 * Build + run via crate/abgen-native/tests/run-sanity.sh.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "abgen.h"

struct capture {
    int json_events;
    int outputs;
    int errors;
    int manifests;
    size_t largest_output;
    char first_error[512];
    char manifest[4096];
};

static void put_u32(unsigned char **p, uint32_t v)
{
    (*p)[0] = (unsigned char)(v & 0xff);
    (*p)[1] = (unsigned char)((v >> 8) & 0xff);
    (*p)[2] = (unsigned char)((v >> 16) & 0xff);
    (*p)[3] = (unsigned char)((v >> 24) & 0xff);
    *p += 4;
}

static void put_bytes(unsigned char **p, const void *b, size_t n)
{
    put_u32(p, (uint32_t)n);
    memcpy(*p, b, n);
    *p += n;
}

static uint32_t read_u32(const uint8_t *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
           ((uint32_t)p[3] << 24);
}

static void on_emit(void *ud, uint32_t kind, const uint8_t *ptr, size_t len)
{
    struct capture *c = ud;

    switch ((AbgenKind)kind) {
    case ABGEN_KIND_JSON:
        c->json_events++;
        break;
    case ABGEN_KIND_OUTPUT: {
        c->outputs++;
        if (len < 4) break;
        uint32_t name_len = read_u32(ptr);
        /* size_t: 4 + name_len + 4 wraps in unsigned int for a large name_len */
        if (len < (size_t)name_len + 8) break;
        uint32_t data_len = read_u32(ptr + 4 + name_len);
        if (data_len > c->largest_output) c->largest_output = data_len;
        printf("  output: %.*s (%u bytes)\n", (int)name_len, ptr + 4, data_len);
        break;
    }
    case ABGEN_KIND_ERROR:
        c->errors++;
        if (c->first_error[0] == '\0' && len < sizeof(c->first_error)) {
            memcpy(c->first_error, ptr, len);
            c->first_error[len] = '\0';
        }
        break;
    case ABGEN_KIND_MANIFEST:
        c->manifests++;
        if (len < sizeof(c->manifest)) {
            memcpy(c->manifest, ptr, len);
            c->manifest[len] = '\0';
        }
        break;
    }
}

static unsigned char *read_file(const char *path, size_t *out_len)
{
    FILE *f = fopen(path, "rb");
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (n <= 0) { fclose(f); return NULL; }
    unsigned char *buf = malloc((size_t)n);
    if (!buf) { fclose(f); return NULL; }
    if (fread(buf, 1, (size_t)n, f) != (size_t)n) {
        free(buf); fclose(f); return NULL;
    }
    fclose(f);
    *out_len = (size_t)n;
    return buf;
}

static unsigned char *build_request(const char *name, const unsigned char *glb,
                                    size_t glb_len, const char *platform,
                                    uint8_t mode, size_t *out_len)
{
    size_t name_len = strlen(name);
    size_t plat_len = strlen(platform);
    size_t cap = 4 + (4 + name_len) + (4 + glb_len) + (4 + plat_len) + 4 + 4 +
                 4 + 4 + 4 + 4 + 64;
    unsigned char *buf = malloc(cap);
    if (!buf) return NULL;
    unsigned char *p = buf;

    put_u32(&p, 1);                       /* file_count */
    put_bytes(&p, name, name_len);
    put_bytes(&p, glb, glb_len);
    put_bytes(&p, platform, plat_len);
    put_bytes(&p, "", 0);                 /* entity_type: detect */
    *p++ = 0;                             /* magenta_missing */
    *p++ = 0;                             /* bake_lod */
    *p++ = mode;
    *p++ = 0;                             /* crop */
    put_u32(&p, 0);                       /* tri_cap */
    put_bytes(&p, "", 0);                 /* entity_hash */
    put_bytes(&p, "", 0);                 /* only_glb */
    put_u32(&p, 0);                       /* content table */

    *out_len = (size_t)(p - buf);
    return buf;
}

static int failures = 0;

static void check(int cond, const char *what)
{
    printf("%s %s\n", cond ? "  ok  " : "  FAIL", what);
    if (!cond) failures++;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <path/to/model.glb>\n", argv[0]);
        return 2;
    }

    printf("abgen C ABI sanity test\n");
    printf("  abi_version = %u (header says %u)\n", abgen_abi_version(),
           ABGEN_ABI_VERSION);
    printf("  version     = %s\n", abgen_version());

    check(abgen_abi_version() == ABGEN_ABI_VERSION,
          "library ABI matches the header");

    int32_t tc = abgen_set_max_threads(4);
    check(tc == ABGEN_OK || tc == ABGEN_ERR_ALREADY_CONFIGURED,
          "abgen_set_max_threads accepted");

    check(abgen_convert(NULL, 0, on_emit, NULL) == ABGEN_ERR_NULL_ARG,
          "null request rejected");
    {
        unsigned char one = 0;
        check(abgen_convert(&one, 1, NULL, NULL) == ABGEN_ERR_NULL_ARG,
              "null callback rejected");
    }

    {
        struct capture c;
        memset(&c, 0, sizeof c);
        unsigned char junk[5] = { 0xff, 0xff, 0xff, 0xff, 0x00 };
        int32_t rc = abgen_convert(junk, sizeof junk, on_emit, &c);
        check(rc == ABGEN_ERR_MALFORMED_INPUT, "malformed request rejected");
        check(c.errors == 1, "malformed request reported one error");
        printf("       error text: %s\n", c.first_error);
    }

    size_t glb_len = 0;
    unsigned char *glb = read_file(argv[1], &glb_len);
    if (!glb) {
        fprintf(stderr, "could not read %s\n", argv[1]);
        return 2;
    }
    printf("  source: %s (%zu bytes)\n", argv[1], glb_len);

    {
        struct capture c;
        memset(&c, 0, sizeof c);
        size_t req_len = 0;
        unsigned char *req = build_request("model.glb", glb, glb_len, "windows",
                                           ABGEN_MODE_CONVERT, &req_len);
        if (!req) { fprintf(stderr, "oom\n"); return 2; }

        int32_t rc = abgen_convert(req, req_len, on_emit, &c);
        check(rc == ABGEN_OK, "convert returned ABGEN_OK");
        check(c.errors == 0, "convert emitted no fatal errors");
        check(c.outputs >= 1, "convert produced at least one artifact");
        check(c.largest_output > 0, "artifact carries bytes");
        check(c.manifests == 1, "convert emitted exactly one manifest");
        check(strstr(c.manifest, "\"exitCode\":0") != NULL,
              "manifest reports exitCode 0");
        check(strstr(c.manifest, "v-abgen-native") != NULL,
              "manifest identifies the native host");
        printf("  manifest: %s\n", c.manifest);
        printf("  json events: %d\n", c.json_events);
        free(req);
    }

    {
        struct capture c;
        memset(&c, 0, sizeof c);
        size_t req_len = 0;
        unsigned char *req = build_request("model.glb", glb, glb_len, "windows",
                                           ABGEN_MODE_SCAN, &req_len);
        if (!req) { fprintf(stderr, "oom\n"); return 2; }
        int32_t rc = abgen_convert(req, req_len, on_emit, &c);
        check(rc == ABGEN_OK, "scan returned ABGEN_OK");
        check(c.outputs == 0, "scan produced no artifacts");
        check(c.json_events > 0, "scan reported events");
        free(req);
    }

    free(glb);

    printf(failures ? "\nFAILED (%d)\n" : "\nPASS\n", failures);
    return failures ? 1 : 0;
}
