// File: crn_platform.cpp
// See Copyright Notice and license at the end of inc/crnlib.h
#include "crn_core.h"

#if CRNLIB_USE_WIN32_API
#include "crn_winhdr.h"
#endif
#ifndef _MSC_VER
// mingw's UCRT headers ship sprintf_s/vsprintf_s as always-inline overloads
// (sec_api/stdio_s.h, inside #ifdef _UCRT), so defining them here as well is a
// redefinition error. What decides it is the CRT rather than the compiler:
// mingw-w64 >= 12 defaults MINGW_HAS_SECURE_API on, so a UCRT toolchain
// collides whether it is GCC or clang. __clang__ is kept alongside _UCRT
// because the llvm-mingw lane is known to need the guard and there is no
// reason to narrow it to prove a point.
#if !(defined(__MINGW32__) && (defined(__clang__) || defined(_UCRT)))
int sprintf_s(char* buffer, size_t sizeOfBuffer, const char* format, ...) {
  if (!sizeOfBuffer)
    return 0;

  va_list args;
  va_start(args, format);
  int c = vsnprintf(buffer, sizeOfBuffer, format, args);
  va_end(args);

  buffer[sizeOfBuffer - 1] = '\0';

  if (c < 0)
    return sizeOfBuffer - 1;

  return CRNLIB_MIN(c, (int)sizeOfBuffer - 1);
}

int vsprintf_s(char* buffer, size_t sizeOfBuffer, const char* format, va_list args) {
  if (!sizeOfBuffer)
    return 0;

  int c = vsnprintf(buffer, sizeOfBuffer, format, args);

  buffer[sizeOfBuffer - 1] = '\0';

  if (c < 0)
    return sizeOfBuffer - 1;

  return CRNLIB_MIN(c, (int)sizeOfBuffer - 1);
}
#endif

char* strlwr(char* p) {
  char* q = p;
  while (*q) {
    char c = *q;
    *q++ = tolower(c);
  }
  return p;
}

char* strupr(char* p) {
  char* q = p;
  while (*q) {
    char c = *q;
    *q++ = toupper(c);
  }
  return p;
}
#endif  // __GNUC__

void crnlib_debug_break(void) {
  CRNLIB_BREAKPOINT
}

#if CRNLIB_USE_WIN32_API
#include "crn_winhdr.h"

bool crnlib_is_debugger_present(void) {
  return IsDebuggerPresent() != 0;
}

void crnlib_output_debug_string(const char* p) {
  OutputDebugStringA(p);
}
#else
bool crnlib_is_debugger_present(void) {
  return false;
}

void crnlib_output_debug_string(const char* p) {
  puts(p);
}
#endif  // CRNLIB_USE_WIN32_API
