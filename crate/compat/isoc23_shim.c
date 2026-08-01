/* Keep libabgen.so's glibc floor at 2.34 instead of 2.38.
 *
 * glibc 2.38+ headers redirect the strtol/sscanf families to __isoc23_*
 * symbols whenever C23 semantics are in scope, and g++ defines _GNU_SOURCE
 * unconditionally, which sets __GLIBC_USE_C23_STRTOL via features.h. So every
 * C++ translation unit in the vendored native code picks the redirect up on
 * any build host with glibc >= 2.38 — this is not a nix artifact, the rustup
 * ubuntu-24.04 leg produces the same symbols. Two verneed entries then raise
 * the whole library's floor from 2.34 to 2.38 and exclude Ubuntu 22.04 LTS,
 * Debian 12, RHEL 9, Amazon Linux 2023 and the Steam Runtime 3 sniper
 * container — a stricter floor than Unity 6's own stated Linux minimum.
 *
 * abgen-host does not need this: it ships its own glibc. A library dlopen'd
 * into Unity uses the host process's glibc and cannot.
 *
 * The C23 delta and why it is inert here. Measured, not assumed: exactly five
 * objects reference these symbols, and every call site is base 10 or a format
 * C23 does not touch.
 *
 *   crn_stb_image.o  crunch    strtol(token, &token, 10)
 *   static.o         mimalloc  strtol(buf, &end, 10)
 *   options.cc.o     draco     strtol(act_str, &next_str, 10)
 *   ply_reader.cc.o  draco     strtoll(count_str.c_str(), nullptr, 10)
 *   jmemmgr.o        libjpeg   sscanf(memenv, "%ld%c", &max_to_use, &ch)
 *
 * For strtol the only C23 change is accepting a 0b/0B prefix at base 0 and
 * base 2; at base 10 the two entry points agree on every input, including
 * "0b101" (both stop at 'b'). For scanf the change is the %b/%B conversions
 * and a 0b prefix for %i; "%ld%c" is unaffected.
 *
 * One latent site to know about: crunch's crn_value.h has sscanf(p, "%i,...")
 * and %i *is* C23-affected. It is not reached — no crunch object references
 * __isoc23_sscanf — but if it ever becomes reached, the shim makes it parse
 * the way it already does on every host below glibc 2.38 rather than the way
 * this one build host would. Uniform across hosts is the point.
 *
 * No glibc headers here: including <stdlib.h>/<stdio.h> would redirect the
 * very calls this file forwards, turning each shim into infinite recursion.
 * The plain symbols are declared by hand so they bind to their original
 * default versions (strtol@GLIBC_2.2.5 and friends).
 *
 * hidden visibility resolves the references inside the link unit without
 * exporting the shims from libabgen.so. crate/build.rs force-links the object
 * (+whole-archive) because the references live in four unrelated archives and
 * on-demand archive pull-in would depend on link order.
 */
#include <stdarg.h>

extern long strtol(const char *, char **, int);
extern long long strtoll(const char *, char **, int);
extern unsigned long strtoul(const char *, char **, int);
extern unsigned long long strtoull(const char *, char **, int);
extern int vsscanf(const char *, const char *, va_list);

#define ABGEN_SHIM __attribute__((visibility("hidden")))

ABGEN_SHIM long __isoc23_strtol(const char *s, char **end, int base) {
    return strtol(s, end, base);
}

ABGEN_SHIM long long __isoc23_strtoll(const char *s, char **end, int base) {
    return strtoll(s, end, base);
}

ABGEN_SHIM unsigned long __isoc23_strtoul(const char *s, char **end, int base) {
    return strtoul(s, end, base);
}

ABGEN_SHIM unsigned long long __isoc23_strtoull(const char *s, char **end,
                                                int base) {
    return strtoull(s, end, base);
}

ABGEN_SHIM int __isoc23_sscanf(const char *s, const char *fmt, ...) {
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = vsscanf(s, fmt, ap);
    va_end(ap);
    return r;
}

ABGEN_SHIM int __isoc23_vsscanf(const char *s, const char *fmt, va_list ap) {
    return vsscanf(s, fmt, ap);
}
