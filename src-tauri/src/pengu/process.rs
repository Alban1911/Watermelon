use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

const TARGET_NAME: &str = "LeagueClientUx.exe";

/// Best-effort: kills every running `LeagueClientUx.exe`. The parent
/// `LeagueClient.exe` will respawn them, which is what makes (or unmakes)
/// the IFEO hook take effect on a live session.
pub fn terminate_league_client_ux() {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) == 0 {
            CloseHandle(snapshot);
            return;
        }

        loop {
            if exe_name_matches(&entry.szExeFile, TARGET_NAME) {
                let handle = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                if !handle.is_null() {
                    TerminateProcess(handle, 0);
                    CloseHandle(handle);
                }
            }
            if Process32NextW(snapshot, &mut entry) == 0 {
                break;
            }
        }
        CloseHandle(snapshot);
    }
}

fn exe_name_matches(sz_exe_file: &[u16; 260], target: &str) -> bool {
    let len = sz_exe_file
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(sz_exe_file.len());
    String::from_utf16_lossy(&sz_exe_file[..len]) == target
}
