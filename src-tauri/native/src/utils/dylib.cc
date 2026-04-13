#include "pengu.h"
#include <algorithm>
#include <vector>

void *dylib::find_lib(const char *name)
{
    return (void *)GetModuleHandleA(name);
}

void *dylib::find_proc(void *lib, const char *proc)
{
    return (void *)GetProcAddress((HMODULE)lib, proc);
}

static auto pattern_to_bytes(const char *pattern, bool *wildcard)
{
    *wildcard = false;
    std::vector<int> bytes;
    const char *end = pattern + strlen(pattern);

    for (const char *cur = pattern; cur < end; ++cur)
    {
        if (*cur == '?')
        {
            ++cur;
            if (*cur == '?')
                ++cur;
            bytes.push_back(-1);
            *wildcard = true;
        }
        else
        {
            char *eptr;
            bytes.push_back(strtol(cur, &eptr, 16));
            cur = eptr;
        }
    }
    return bytes;
}

static void *scan_memory_pattern(void *data, size_t length, const std::vector<int> &pattern)
{
    size_t pattern_size = pattern.size();
    auto pattern_bytes = pattern.data();

    size_t find_size = length - pattern_size;
    auto scan_bytes = reinterpret_cast<uint8_t *>(data);

    for (size_t i = 0; i < find_size; ++i)
    {
        for (size_t j = 0; j < pattern_size; ++j)
            if (scan_bytes[i + j] != pattern_bytes[j] && pattern_bytes[j] != -1)
                goto next;
        return scan_bytes + i;
    next:;
    }

    return nullptr;
}

static void *scan_memory_bytes(void *data, size_t length, const std::vector<int> &find_bytes)
{
    auto image_start = (uint8_t *)data;
    auto image_end = image_start + length;

    auto occurrence = std::search(image_start, image_end, find_bytes.begin(), find_bytes.end());
    return (occurrence != image_end) ? occurrence : nullptr;
}

static void *get_base_address(const void *rladdr)
{
    HMODULE module;
    DWORD flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if (GetModuleHandleExA(flags, (LPCSTR)rladdr, &module))
    {
        return (void *)module;
    }
    return nullptr;
}

static size_t get_lib_size(void *base_address)
{
    auto dos_header = (IMAGE_DOS_HEADER *)base_address;
    auto nt_headers = (IMAGE_NT_HEADERS *)((uint8_t *)(base_address) + dos_header->e_lfanew);
    return nt_headers->OptionalHeader.SizeOfImage;
}

void *dylib::find_memory(const void *rladdr, const char *pattern)
{
    bool wildcard;
    auto pattern_bytes = pattern_to_bytes(pattern, &wildcard);
    if (!pattern_bytes.size())
        return nullptr;

    void *base_address = get_base_address(rladdr);
    if (!base_address)
        return nullptr;

    size_t lib_size = get_lib_size(base_address);
    if (!lib_size)
        return nullptr;

    if (wildcard)
        return scan_memory_pattern(base_address, lib_size, pattern_bytes);
    else
        return scan_memory_bytes(base_address, lib_size, pattern_bytes);
}
