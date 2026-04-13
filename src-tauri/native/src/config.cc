#include "pengu.h"
#include <fstream>
#include <unordered_map>
#include "include/cef_version.h"

path config::loader_dir()
{
    static std::wstring dir_path;
    if (dir_path.empty())
    {
        WCHAR thisPath[2048];
        GetModuleFileNameW(g_hModule, thisPath, COUNT_OF(thisPath));

        DWORD attr = GetFileAttributesW(thisPath);
        if ((attr & FILE_ATTRIBUTE_REPARSE_POINT) != FILE_ATTRIBUTE_REPARSE_POINT)
        {
            dir_path = thisPath;
            dir_path = dir_path.substr(0, dir_path.find_last_of(L"/\\"));
            return dir_path;
        }

        WCHAR finalPath[2048];
        HANDLE file = CreateFileW(thisPath, GENERIC_READ, 0x1, NULL, OPEN_EXISTING, 0, NULL);
        DWORD pathLength = GetFinalPathNameByHandleW(file, finalPath, 2048, FILE_NAME_OPENED);
        CloseHandle(file);

        std::wstring dir{ finalPath, pathLength };
        if (dir.rfind(L"\\\\?\\", 0) == 0)
            dir.erase(0, 4);

        dir_path = dir.substr(0, dir.find_last_of(L"/\\"));
    }
    return dir_path;
}

path config::datastore_path()
{
    return loader_dir() / "datastore";
}

path config::cache_dir()
{
    wchar_t lpath[2048];
    size_t length = GetEnvironmentVariableW(L"LOCALAPPDATA", lpath, COUNT_OF(lpath));

    if (length == 0)
        return league_dir() / "Cache";

    lstrcatW(lpath, L"\\Riot Games\\League of Legends\\Cache");
    return lpath;
}

path config::league_dir()
{
    wchar_t buf[2048];
    size_t length = GetModuleFileNameW(nullptr, buf, COUNT_OF(buf));

    std::wstring lpath(buf, length);
    return lpath.substr(0, lpath.find_last_of(L"/\\"));
}

static void trim_string(std::string &str)
{
    str.erase(str.find_last_not_of(' ') + 1);
    str.erase(0, str.find_first_not_of(' '));
}

static auto get_config_map()
{
    static bool cached = false;
    static std::unordered_map<std::string, std::string> map;

    if (!cached)
    {
        auto cpath = config::loader_dir() / "config";
        std::ifstream file(cpath);

        if (file.is_open())
        {
            std::string line;
            while (std::getline(file, line))
            {
                if (line.empty() || line[0] == ';' || line[0] == '#')
                    continue;

                size_t pos = line.find('=');
                if (pos != std::string::npos)
                {
                    std::string key = line.substr(0, pos);
                    std::string value = line.substr(pos + 1);
                    trim_string(key);
                    trim_string(value);
                    map[key] = value;
                }
            }
            file.close();
        }

        cached = true;
    }

    return map;
}

static std::string get_config_value(const char *key, const char *fallback)
{
    auto map = get_config_map();
    auto it = map.find(key);
    return (it != map.end()) ? it->second : std::string(fallback);
}

static bool get_config_value_bool(const char *key, bool fallback)
{
    auto map = get_config_map();
    auto it = map.find(key);
    if (it != map.end())
    {
        if (it->second == "0" || it->second == "false")
            return false;
        if (it->second == "1" || it->second == "true")
            return true;
    }
    return fallback;
}

static int get_config_value_int(const char *key, int fallback)
{
    auto map = get_config_map();
    auto it = map.find(key);
    if (it != map.end())
        return std::stoi(it->second);
    return fallback;
}

path config::plugins_dir()
{
    std::string cpath = get_config_value("plugins_dir", "");
    if (!cpath.empty())
        return (const char8_t *)cpath.c_str();

    return loader_dir() / "plugins";
}

std::string config::disabled_plugins()
{
    return get_config_value("disabled_plugins", "");
}

namespace config::options
{
    bool optimized_client()
    {
        return get_config_value_bool("OptimizeClient", true);
    }

    bool super_potato()
    {
        return get_config_value_bool("SuperLowSpecMode", false);
    }

    bool isecure_mode()
    {
        return get_config_value_bool("DisableWebSecurity", false);
    }

    int debug_port()
    {
        return get_config_value_int("RemoteDebuggingPort", 0);
    }
}
