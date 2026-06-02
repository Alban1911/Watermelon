#include "pengu.h"
#include "hook.h"
#include "include/cef_version.h"

bool check_libcef_version(bool is_browser);
void HookBrowserProcess();
void HookRendererProcess();

HMODULE g_hModule = nullptr;

static bool wcsfindi(const wchar_t *str, const wchar_t *sub)
{
    size_t str_len = wcslen(str), sub_len = wcslen(sub);
    if (sub_len > str_len)
        return false;
    for (size_t i = 0; i <= str_len - sub_len; ++i) {
        for (size_t j = 0; j < sub_len; ++j)
            if (towlower(str[i + j]) != towlower(sub[j]))
                goto next;
        return true;
        next:;
    }
    return false;
}

static hook::Hook<decltype(&CreateProcessW)> Old_CreateProcessW;
static BOOL WINAPI Hooked_CreateProcessW(LPCWSTR lpApplicationName, LPWSTR lpCommandLine,
    LPSECURITY_ATTRIBUTES lpProcessAttributes, LPSECURITY_ATTRIBUTES lpThreadAttributes,
    BOOL bInheritHandles, DWORD dwCreationFlags, LPVOID lpEnvironment, LPCWSTR lpCurrentDirectory,
    LPSTARTUPINFOW lpStartupInfo, LPPROCESS_INFORMATION lpProcessInformation)
{
    bool is_renderer = wcsfindi(lpCommandLine, L"LeagueClientUxRender.exe")
        && wcsfindi(lpCommandLine, L"--type=renderer");

    if (is_renderer)
        dwCreationFlags |= CREATE_SUSPENDED;

    BOOL success = Old_CreateProcessW(lpApplicationName, lpCommandLine, lpProcessAttributes, lpThreadAttributes,
        bInheritHandles, dwCreationFlags, lpEnvironment, lpCurrentDirectory, lpStartupInfo, lpProcessInformation);

    if (success && is_renderer)
    {
        void InjectThisDll(HANDLE hProcess);
        InjectThisDll(lpProcessInformation->hProcess);
        ResumeThread(lpProcessInformation->hThread);
    }

    return success;
}

static void Initialize()
{
    WCHAR exe_path[2048]{};
    GetModuleFileNameW(nullptr, exe_path, COUNT_OF(exe_path));

    // Browser process.
    if (wcsfindi(exe_path, L"LeagueClientUx.exe"))
    {
        OutputDebugStringA("[Watermelon] Browser process detected");
        if (check_libcef_version(true))
        {
            HookBrowserProcess();
            Old_CreateProcessW.hook(&CreateProcessW, Hooked_CreateProcessW);
            OutputDebugStringA("[Watermelon] CreateProcessW hooked");
        }
    }
    // Render process.
    else if (wcsfindi(exe_path, L"LeagueClientUxRender.exe"))
    {
        if (wcsstr(GetCommandLineW(), L"--type=renderer") != nullptr)
        {
            OutputDebugStringA("[Watermelon] Renderer process detected");
            if (check_libcef_version(false))
            {
                HookRendererProcess();
                OutputDebugStringA("[Watermelon] Renderer hooks installed");
            }
        }
    }
}

// DLL entry point.
BOOL APIENTRY DllMain(HMODULE module, DWORD reason, LPVOID reserved)
{
    switch (reason)
    {
        case DLL_PROCESS_ATTACH:
            g_hModule = module;
            DisableThreadLibraryCalls(module);
            OutputDebugStringA("[Watermelon] DllMain: DLL_PROCESS_ATTACH");
            Initialize();
            break;

        case DLL_THREAD_ATTACH:
        case DLL_THREAD_DETACH:
        case DLL_PROCESS_DETACH:
            break;
    }

    return TRUE;
}

void InjectThisDll(HANDLE hProcess)
{
    HMODULE kernel32 = GetModuleHandleA("kernel32");
    auto pVirtualAllocEx = (decltype(&VirtualAllocEx))GetProcAddress(kernel32, "VirtualAllocEx");
    auto pWriteProcessMemory = (decltype(&WriteProcessMemory))GetProcAddress(kernel32, "WriteProcessMemory");
    auto pCreateRemoteThread = (decltype(&CreateRemoteThread))GetProcAddress(kernel32, "CreateRemoteThread");

    WCHAR thisDllPath[2048]{};
    GetModuleFileNameW(g_hModule, thisDllPath, COUNT_OF(thisDllPath));

    size_t pathSize = (wcslen(thisDllPath) + 1) * sizeof(WCHAR);
    LPVOID pathAddr = pVirtualAllocEx(hProcess, NULL, pathSize, MEM_COMMIT, PAGE_READWRITE);
    pWriteProcessMemory(hProcess, pathAddr, thisDllPath, pathSize, NULL);

    HANDLE loader = pCreateRemoteThread(hProcess, NULL, 0, (LPTHREAD_START_ROUTINE)&LoadLibraryW, pathAddr, 0, NULL);
    WaitForSingleObject(loader, INFINITE);
    CloseHandle(loader);
}

extern "C" __declspec(dllexport) int APIENTRY _BootstrapEntryW(HWND, HINSTANCE, LPWSTR commandLine, int)
{
    LONG (NTAPI *NtQueryInformationProcess)(HANDLE, DWORD, PVOID, ULONG, PULONG);
    LONG (NTAPI *NtRemoveProcessDebug)(HANDLE, HANDLE);
    LONG (NTAPI *NtClose)(HANDLE Handle);

    OutputDebugStringA("[Watermelon] _BootstrapEntry called");

    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);

    if (!CreateProcessW(NULL, commandLine, NULL, NULL, FALSE,
        CREATE_SUSPENDED | DEBUG_ONLY_THIS_PROCESS, NULL, NULL, &si, &pi))
    {
        char msg[128];
        snprintf(msg, sizeof(msg), "Failed to create LeagueClientUx process, last error: 0x%08X.", (unsigned)GetLastError());
        MessageBoxA(NULL, msg, "Watermelon bootstrapper", MB_ICONWARNING | MB_OK | MB_TOPMOST);
        return 1;
    }

    HMODULE ntdll = GetModuleHandleA("ntdll");
    NtQueryInformationProcess = reinterpret_cast<decltype(NtQueryInformationProcess)>(GetProcAddress(ntdll, "NtQueryInformationProcess"));
    NtRemoveProcessDebug = reinterpret_cast<decltype(NtRemoveProcessDebug)>(GetProcAddress(ntdll, "NtRemoveProcessDebug"));
    NtClose = reinterpret_cast<decltype(NtClose)>(GetProcAddress(ntdll, "NtClose"));

    HANDLE hDebug;
    if (NtQueryInformationProcess(pi.hProcess, 30, &hDebug, sizeof(HANDLE), 0) >= 0)
    {
        NtRemoveProcessDebug(pi.hProcess, hDebug);
        NtClose(hDebug);
    }

    InjectThisDll(pi.hProcess);
    ResumeThread(pi.hThread);
    WaitForSingleObject(pi.hProcess, INFINITE);

    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    return 0;
}

extern "C" __declspec(dllexport) int _GetCefVersion()
{
    return CEF_VERSION_MAJOR;
}
