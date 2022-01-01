#![cfg(windows)]
use super::*;

impl LocalProcessInfo {
    pub fn with_root_pid(pid: u32) -> Option<Self> {
        use ntapi::ntpebteb::PEB;
        use ntapi::ntpsapi::{
            NtQueryInformationProcess, ProcessBasicInformation, ProcessWow64Information,
            PROCESS_BASIC_INFORMATION,
        };
        use ntapi::ntrtl::RTL_USER_PROCESS_PARAMETERS;
        use ntapi::ntwow64::RTL_USER_PROCESS_PARAMETERS32;
        use std::ffi::OsString;
        use std::mem::MaybeUninit;
        use std::os::windows::ffi::OsStringExt;
        use winapi::shared::minwindef::{FILETIME, HMODULE, LPVOID, MAX_PATH};
        use winapi::shared::ntdef::{FALSE, NT_SUCCESS};
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::memoryapi::ReadProcessMemory;
        use winapi::um::processthreadsapi::{GetProcessTimes, OpenProcess};
        use winapi::um::psapi::{EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL};
        use winapi::um::shellapi::CommandLineToArgvW;
        use winapi::um::tlhelp32::*;
        use winapi::um::winbase::LocalFree;
        use winapi::um::winnt::{HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

        struct Snapshot(HANDLE);

        impl Snapshot {
            pub fn new() -> Option<Self> {
                let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
                if handle.is_null() {
                    None
                } else {
                    Some(Self(handle))
                }
            }

            pub fn iter(&self) -> ProcIter {
                ProcIter {
                    snapshot: &self,
                    first: true,
                }
            }
        }

        impl Drop for Snapshot {
            fn drop(&mut self) {
                unsafe { CloseHandle(self.0) };
            }
        }

        struct ProcIter<'a> {
            snapshot: &'a Snapshot,
            first: bool,
        }

        impl<'a> Iterator for ProcIter<'a> {
            type Item = PROCESSENTRY32W;

            fn next(&mut self) -> Option<Self::Item> {
                let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as _;
                let res = if self.first {
                    self.first = false;
                    unsafe { Process32FirstW(self.snapshot.0, &mut entry) }
                } else {
                    unsafe { Process32NextW(self.snapshot.0, &mut entry) }
                };
                if res == 0 {
                    None
                } else {
                    Some(entry)
                }
            }
        }

        let snapshot = Snapshot::new()?;
        let procs: Vec<_> = snapshot.iter().collect();

        fn wstr_to_path(slice: &[u16]) -> PathBuf {
            match slice.iter().position(|&c| c == 0) {
                Some(nul) => OsString::from_wide(&slice[..nul]),
                None => OsString::from_wide(slice),
            }
            .into()
        }
        fn wstr_to_string(slice: &[u16]) -> String {
            wstr_to_path(slice).to_string_lossy().into_owned()
        }

        struct ProcParams {
            argv: Vec<String>,
            cwd: PathBuf,
        }

        struct ProcHandle(HANDLE);
        impl ProcHandle {
            fn new(pid: u32) -> Option<Self> {
                let options = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
                let handle = unsafe { OpenProcess(options, FALSE as _, pid) };
                if handle.is_null() {
                    return None;
                }
                Some(Self(handle))
            }

            fn hmodule(&self) -> Option<HMODULE> {
                let mut needed = 0;
                let mut hmod = [0 as HMODULE];
                let size = std::mem::size_of_val(&hmod);
                let res = unsafe {
                    EnumProcessModulesEx(
                        self.0,
                        hmod.as_mut_ptr(),
                        size as _,
                        &mut needed,
                        LIST_MODULES_ALL,
                    )
                };
                if res == 0 {
                    None
                } else {
                    Some(hmod[0])
                }
            }

            fn executable(&self) -> Option<PathBuf> {
                let hmod = self.hmodule()?;
                let mut buf = [0u16; MAX_PATH + 1];
                let res =
                    unsafe { GetModuleFileNameExW(self.0, hmod, buf.as_mut_ptr(), buf.len() as _) };
                if res == 0 {
                    None
                } else {
                    Some(wstr_to_path(&buf))
                }
            }

            fn get_peb32_addr(&self) -> Option<LPVOID> {
                let mut peb32_addr = MaybeUninit::<LPVOID>::uninit();
                let res = unsafe {
                    NtQueryInformationProcess(
                        self.0,
                        ProcessWow64Information,
                        peb32_addr.as_mut_ptr() as _,
                        std::mem::size_of::<LPVOID>() as _,
                        std::ptr::null_mut(),
                    )
                };
                if !NT_SUCCESS(res) {
                    return None;
                }
                let peb32_addr = unsafe { peb32_addr.assume_init() };
                if peb32_addr.is_null() {
                    None
                } else {
                    Some(peb32_addr)
                }
            }

            fn get_params(&self) -> Option<ProcParams> {
                match self.get_peb32_addr() {
                    Some(peb32) => self.get_params_32(peb32),
                    None => self.get_params_64(),
                }
            }

            fn get_basic_info(&self) -> Option<PROCESS_BASIC_INFORMATION> {
                let mut info = MaybeUninit::<PROCESS_BASIC_INFORMATION>::uninit();
                let res = unsafe {
                    NtQueryInformationProcess(
                        self.0,
                        ProcessBasicInformation,
                        info.as_mut_ptr() as _,
                        std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as _,
                        std::ptr::null_mut(),
                    )
                };
                if !NT_SUCCESS(res) {
                    return None;
                }
                let info = unsafe { info.assume_init() };
                Some(info)
            }

            fn read_struct<T>(&self, addr: LPVOID) -> Option<T> {
                let mut data = MaybeUninit::<T>::uninit();
                let res = unsafe {
                    ReadProcessMemory(
                        self.0,
                        addr as _,
                        data.as_mut_ptr() as _,
                        std::mem::size_of::<T>() as _,
                        std::ptr::null_mut(),
                    )
                };
                if res == 0 {
                    return None;
                }
                let data = unsafe { data.assume_init() };
                Some(data)
            }

            fn get_peb(&self, info: &PROCESS_BASIC_INFORMATION) -> Option<PEB> {
                self.read_struct(info.PebBaseAddress as _)
            }

            fn get_proc_params(&self, peb: &PEB) -> Option<RTL_USER_PROCESS_PARAMETERS> {
                self.read_struct(peb.ProcessParameters as _)
            }

            fn get_params_64(&self) -> Option<ProcParams> {
                let info = self.get_basic_info()?;
                let peb = self.get_peb(&info)?;
                let params = self.get_proc_params(&peb)?;

                let cmdline = self.read_process_wchar(
                    params.CommandLine.Buffer as _,
                    params.CommandLine.Length as _,
                )?;
                let cwd = self.read_process_wchar(
                    params.CurrentDirectory.DosPath.Buffer as _,
                    params.CurrentDirectory.DosPath.Length as _,
                )?;

                Some(ProcParams {
                    argv: cmd_line_to_argv(&cmdline),
                    cwd: wstr_to_path(&cwd),
                })
            }

            fn get_proc_params_32(&self, peb32: LPVOID) -> Option<RTL_USER_PROCESS_PARAMETERS32> {
                self.read_struct(peb32)
            }

            fn get_params_32(&self, peb32: LPVOID) -> Option<ProcParams> {
                let params = self.get_proc_params_32(peb32)?;

                let cmdline = self.read_process_wchar(
                    params.CommandLine.Buffer as _,
                    params.CommandLine.Length as _,
                )?;
                let cwd = self.read_process_wchar(
                    params.CurrentDirectory.DosPath.Buffer as _,
                    params.CurrentDirectory.DosPath.Length as _,
                )?;

                Some(ProcParams {
                    argv: cmd_line_to_argv(&cmdline),
                    cwd: wstr_to_path(&cwd),
                })
            }

            fn read_process_wchar(&self, ptr: LPVOID, size: usize) -> Option<Vec<u16>> {
                let mut buf = vec![0u16; size / 2];

                let res = unsafe {
                    ReadProcessMemory(
                        self.0,
                        ptr as _,
                        buf.as_mut_ptr() as _,
                        size,
                        std::ptr::null_mut(),
                    )
                };
                if res == 0 {
                    return None;
                }

                Some(buf)
            }

            fn start_time(&self) -> Option<SystemTime> {
                let mut start = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let mut exit = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let mut kernel = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let mut user = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let res = unsafe {
                    GetProcessTimes(self.0, &mut start, &mut exit, &mut kernel, &mut user)
                };
                if res == 0 {
                    return None;
                }

                // Units are 100 nanoseconds
                let start = (start.dwHighDateTime as u64) << 32 | start.dwLowDateTime as u64;
                let start = Duration::from_nanos(start * 100);

                // Difference between the windows epoch and the unix epoch
                const WINDOWS_EPOCH: Duration = Duration::from_secs(11_644_473_600);

                Some(SystemTime::UNIX_EPOCH + start - WINDOWS_EPOCH)
            }
        }

        fn cmd_line_to_argv(buf: &[u16]) -> Vec<String> {
            let mut argc = 0;
            let argvp = unsafe { CommandLineToArgvW(buf.as_ptr(), &mut argc) };
            if argvp.is_null() {
                return vec![];
            }

            let argv = unsafe { std::slice::from_raw_parts(argvp, argc as usize) };
            let mut args = vec![];
            for &arg in argv {
                let len = unsafe { libc::wcslen(arg) };
                let arg = unsafe { std::slice::from_raw_parts(arg, len) };
                args.push(wstr_to_string(arg));
            }
            unsafe { LocalFree(argvp as _) };
            args
        }

        impl Drop for ProcHandle {
            fn drop(&mut self) {
                unsafe { CloseHandle(self.0) };
            }
        }

        fn build_proc(info: &PROCESSENTRY32W, procs: &[PROCESSENTRY32W]) -> LocalProcessInfo {
            let mut children = HashMap::new();

            for kid in procs {
                if kid.th32ParentProcessID == info.th32ProcessID {
                    children.insert(kid.th32ProcessID, build_proc(kid, procs));
                }
            }

            let mut executable = wstr_to_path(&info.szExeFile);

            let name = match executable.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => String::new(),
            };

            let mut start_time = SystemTime::now();
            let mut cwd = PathBuf::new();
            let mut argv = vec![];

            if let Some(proc) = ProcHandle::new(info.th32ProcessID) {
                if let Some(exe) = proc.executable() {
                    executable = exe;
                }
                if let Some(params) = proc.get_params() {
                    cwd = params.cwd;
                    argv = params.argv;
                }
                if let Some(start) = proc.start_time() {
                    start_time = start;
                }
            }

            LocalProcessInfo {
                pid: info.th32ProcessID,
                ppid: info.th32ParentProcessID,
                name,
                executable,
                cwd,
                argv,
                start_time,
                status: LocalProcessStatus::Run,
                children,
            }
        }

        if let Some(info) = procs.iter().find(|info| info.th32ProcessID == pid) {
            Some(build_proc(info, &procs))
        } else {
            None
        }
    }
}