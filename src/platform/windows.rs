//! Windows process ownership and Git for Windows discovery.
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    },
};

pub fn bash() -> io::Result<PathBuf> {
    for variable in ["TOKEN_BURN_BASH", "CLAUDE_CODE_GIT_BASH_PATH"] {
        if let Some(path) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            return git_bash(Path::new(&path)).ok_or_else(|| {
                io::Error::other(format!(
                    "{variable} must point to Git for Windows bin/bash.exe or usr/bin/bash.exe"
                ))
            });
        }
    }
    let mut candidates = Vec::new();
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for ancestor in directory.ancestors().take(3) {
            candidates.push(ancestor.join("usr/bin/bash.exe"));
        }
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Git/usr/bin/bash.exe"));
            candidates.push(base.join("Programs/Git/usr/bin/bash.exe"));
        }
    }
    candidates.into_iter().find_map(|path| git_bash(&path)).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound,
        "Git for Windows Bash was not found. Install Git for Windows or set TOKEN_BURN_BASH to its bin/bash.exe"))
}

pub fn spawn_detached(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW};
    let flags = CREATE_NO_WINDOW;
    match command
        .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(error) if error.raw_os_error() == Some(5) => {
            let child = command.creation_flags(flags).spawn()?;
            eprintln!(
                "Warning: Windows refused job breakaway; closing the parent job's host may stop this run"
            );
            Ok(child)
        }
        result => result,
    }
}

pub fn initialize() -> io::Result<()> {
    use windows_sys::Win32::System::{
        Console::SetConsoleCtrlHandler,
        LibraryLoader::{
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            SetDefaultDllDirectories,
        },
    };
    unsafe {
        // Do not load optional ConPTY DLLs from a target repository or its PATH.
        if SetDefaultDllDirectories(
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        // A detached process has no console; in that case there is no handler to reset.
        SetConsoleCtrlHandler(None, 0);
    }
    Ok(())
}

fn git_bash(path: &Path) -> Option<PathBuf> {
    if !path.file_name()?.eq_ignore_ascii_case("bash.exe") {
        return None;
    }
    let parent = path.parent()?;
    let root = if parent.file_name()?.eq_ignore_ascii_case("bin")
        && parent.parent()?.file_name()?.eq_ignore_ascii_case("usr")
    {
        parent.parent()?.parent()?
    } else if parent.file_name()?.eq_ignore_ascii_case("bin") {
        parent.parent()?
    } else {
        return None;
    };
    let bash = root.join("usr/bin/bash.exe");
    (bash.is_file() && root.join("usr/bin/msys-2.0.dll").is_file()).then_some(bash)
}

pub struct Job(HANDLE);
// Job handles are owned, never borrowed, and Windows permits access from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub fn assign(pid: u32) -> io::Result<Arc<Self>> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Arc::new(Self(handle));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let assigned = AssignProcessToJobObject(handle, process);
            let error = io::Error::last_os_error();
            CloseHandle(process);
            if assigned == 0 {
                return Err(error);
            }
            Ok(job)
        }
    }

    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A shell cannot start its payload until it belongs to the job.
pub struct Gate(tempfile::TempDir);

impl Gate {
    pub fn new() -> io::Result<Self> {
        tempfile::tempdir().map(Self)
    }
    pub fn wrap(&self, command: &str) -> String {
        let path = super::shell_path(&self.0.path().join("ready")).replace('\'', "'\\''");
        format!("until [ -f '{path}' ]; do sleep 0.01; done\n{command}")
    }
    pub fn release(&self) -> io::Result<()> {
        std::fs::write(self.0.path().join("ready"), b"")
    }
}
