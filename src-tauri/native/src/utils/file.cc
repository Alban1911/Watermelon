#include "pengu.h"

bool file::is_symlink(const path &p)
{
    DWORD attr = GetFileAttributesW(p.wstring().c_str());
    if (attr == INVALID_FILE_ATTRIBUTES)
        return false;
    return attr & FILE_ATTRIBUTE_REPARSE_POINT;
}

bool file::is_dir(const path &p)
{
    DWORD attr = GetFileAttributesW(p.wstring().c_str());
    if (attr == INVALID_FILE_ATTRIBUTES)
        return false;
    return attr & FILE_ATTRIBUTE_DIRECTORY;
}

bool file::is_file(const path &p)
{
    DWORD attr = GetFileAttributesW(p.wstring().c_str());
    if (attr == INVALID_FILE_ATTRIBUTES)
        return false;
    return !(attr & FILE_ATTRIBUTE_DIRECTORY);
}

bool file::read_file(const path &p, void **buffer, size_t *length)
{
    FILE* fp = _wfopen(p.c_str(), L"rb");
    if (fp != nullptr)
    {
        fseek(fp, 0, SEEK_END);
        long size = ftell(fp);
        fseek(fp, 0, SEEK_SET);

        *buffer = malloc(size + 1);
        if (length) *length = size;

        fread(*buffer, 1, size, fp);
        reinterpret_cast<uint8_t *>(*buffer)[size] = '\0';

        fclose(fp);
        return true;
    }

    return false;
}

bool file::write_file(const path &p, const void *buffer, size_t length)
{
    FILE *fp = _wfopen(p.c_str(), L"wb");
    if (fp != nullptr)
    {
        fwrite(buffer, 1, length, fp);
        fclose(fp);
        return true;
    }
    return false;
}

std::vector<path> file::read_dir(const path &dir)
{
    std::vector<path> files;

    std::wstring target = dir.wstring() + L"\\*";
    WIN32_FIND_DATAW fd;
    HANDLE hFind = FindFirstFileW(target.c_str(), &fd);

    if (hFind != INVALID_HANDLE_VALUE) {
        do {
            files.push_back(fd.cFileName);
        } while (FindNextFileW(hFind, &fd));
        FindClose(hFind);
    }

    return files;
}
