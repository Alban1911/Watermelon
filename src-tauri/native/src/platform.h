#pragma once

#ifdef __cplusplus
class __platform__;
#endif

#if defined(_WIN32) || defined(_WIN64)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef OS_WIN
#define OS_WIN 1
#endif
#ifndef OS_MAC
#define OS_MAC 0
#endif
#else
#error "Only Windows is supported."
#endif

#if !(defined(_M_X64) || defined(_M_AMD64) || defined(__x86_64__) || defined(__amd64__))
#error "Target 64-bit (x86-64/amd64) only."
#endif

#ifndef NOMINMAX
#define NOMINMAX
#endif
#ifndef _CRT_SECURE_NO_WARNINGS
#define _CRT_SECURE_NO_WARNINGS
#endif
#ifndef UNICODE
#define UNICODE 1
#endif

#ifndef COUNT_OF
#define COUNT_OF(arr) (sizeof(arr) / sizeof(*arr))
#endif

#define PLATFORM_NAME "win"
#define LIBCEF_MODULE_NAME "libcef.dll"
