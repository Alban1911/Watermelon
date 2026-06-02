#include "pengu.h"
#include <mutex>

namespace
{
    std::mutex g_log_mutex;

    path log_path()
    {
        return config::appdata_dir() / "logs" / "watermelon-core.log";
    }
}

void logutil::write(const char *message)
{
    if (message == nullptr)
        return;

    OutputDebugStringA(message);

    std::lock_guard<std::mutex> lock(g_log_mutex);

    auto path = log_path();
    std::error_code ec;
    std::filesystem::create_directories(path.parent_path(), ec);

    FILE *file = nullptr;
    if (_wfopen_s(&file, path.c_str(), L"a+b") != 0 || file == nullptr)
        return;

    SYSTEMTIME now{};
    GetLocalTime(&now);
    fprintf(
        file,
        "[%04d-%02d-%02d %02d:%02d:%02d.%03d] %s\n",
        now.wYear,
        now.wMonth,
        now.wDay,
        now.wHour,
        now.wMinute,
        now.wSecond,
        now.wMilliseconds,
        message
    );
    fclose(file);
}
