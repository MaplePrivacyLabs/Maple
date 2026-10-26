#!/usr/bin/env python3
"""Exercise the actual host spawn helper inside a restrictive native job."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
HELPER = SCRIPTS.parent / "app/src/host/windows_spawn.rs"
FIXTURE = r'''
#[path = HELPER_PATH]
mod windows_spawn;

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

type Handle = *mut c_void;
const JOB_OBJECT_ASSIGN_PROCESS: u32 = 0x0001;
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const DETACHED_BREAKAWAY: u32 = 0x00000008 | 0x00000200 | 0x01000000;

#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn OpenJobObjectW(access: u32, inherit: i32, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, size: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
}

struct Job(Handle);

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: Job owns one real handle returned by Create/OpenJobObjectW.
        assert_ne!(unsafe { CloseHandle(self.0) }, 0);
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open_job(name: &OsStr, access: u32) -> Job {
    // SAFETY: name is terminated UTF-16; the returned handle is checked.
    let handle = unsafe { OpenJobObjectW(access, 0, wide(name).as_ptr()) };
    assert!(!handle.is_null(), "open fixture job: {}", io::Error::last_os_error());
    Job(handle)
}

fn assert_membership(job: &Job) {
    let mut member = 0;
    // SAFETY: current-process pseudo-handle, live job handle, writable result.
    assert_ne!(unsafe { IsProcessInJob(GetCurrentProcess(), job.0, &mut member) }, 0,
               "query fixture job: {}", io::Error::last_os_error());
    assert_eq!(member, 1, "helper child escaped the restrictive fixture job");
}

fn probe_command(job: &OsStr) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg("probe").arg(job)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn restricted(job_name: &OsStr) {
    {
        let job = open_job(job_name, JOB_OBJECT_ASSIGN_PROCESS | JOB_OBJECT_QUERY);
        // SAFETY: both handles are live. Only this isolated fixture process is
        // assigned; existing runner jobs and their restrictions are retained.
        assert_ne!(unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) }, 0,
                   "assign restrictive fixture job: {}", io::Error::last_os_error());
        assert_membership(&job);
        // Parent keeps its own handle alive until this worker exits. Releasing
        // this handle cannot kill the worker or leak the job into probe children.
    }
    match probe_command(job_name).creation_flags(DETACHED_BREAKAWAY).spawn() {
        Err(error) => assert_eq!(error.raw_os_error(), Some(5), "direct breakaway error: {error}"),
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the restrictive fixture job unexpectedly permitted breakaway");
        }
    }
    let mut child = windows_spawn::spawn(&mut probe_command(job_name))
        .expect("host helper must retry without breakaway");
    assert!(child.wait().unwrap().success(), "helper child did not retain exact job membership");

    let missing = std::env::current_exe().unwrap().with_file_name("missing-fixture-executable.exe");
    let error = windows_spawn::spawn(&mut Command::new(missing))
        .expect_err("a missing executable must not become a successful host start");
    assert_eq!(error.raw_os_error(), Some(2), "unrelated spawn error must propagate");
    println!("Verified direct breakaway denial, inherited-job host startup and unrelated-error propagation");
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    match arguments.first().and_then(|value| value.to_str()) {
        Some("probe") => {
            let job = open_job(&arguments[1], JOB_OBJECT_QUERY);
            assert_membership(&job);
        }
        Some("restricted") => restricted(&arguments[1]),
        None => {
            let name = format!("Local\\MapleHostSpawn-{}-{}", std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
            // SAFETY: no custom security descriptor; name is terminated UTF-16.
            let wide_name = wide(OsStr::new(&name));
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), wide_name.as_ptr()) };
            let creation_error = unsafe { GetLastError() };
            assert!(!handle.is_null(), "create fixture job: {}", io::Error::last_os_error());
            assert_ne!(creation_error, 183, "fixture job must be newly created");
            let job = Job(handle);
            let mut limits = ExtendedLimitInformation::default();
            // No BREAKAWAY_OK or SILENT_BREAKAWAY_OK. The parent's handle also
            // guarantees cleanup of the worker and children on an outer timeout.
            limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: the C-layout struct is initialized and its size is exact.
            assert_ne!(unsafe { SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &limits as *const _ as *const c_void, std::mem::size_of_val(&limits) as u32) }, 0,
                "set restrictive fixture limits: {}", io::Error::last_os_error());
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("restricted").arg(&name).status().unwrap();
            assert!(status.success(), "restrictive job regression worker failed: {status}");
        }
        Some(other) => panic!("unknown fixture mode: {other}"),
    }
}
'''


class WindowsHostSpawnTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "win32" and shutil.which("rustc"),
                         "restrictive job fixture requires native Windows and Rust")
    def test_host_starts_inside_a_job_that_denies_breakaway(self):
        with tempfile.TemporaryDirectory(prefix="maple-host-job-") as temporary:
            directory = Path(temporary)
            source = directory / "host_job.rs"
            executable = directory / "host-job.exe"
            # Rust's regular string syntax accepts JSON's escaped Windows path.
            source.write_text(FIXTURE.replace("HELPER_PATH", json.dumps(str(HELPER), ensure_ascii=False)), encoding="utf-8")
            command = ["rustc", "--edition=2024", "-D", "warnings", "-C", "opt-level=1"]
            linker = os.environ.get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER")
            if os.environ.get("GITHUB_ACTIONS") == "true":
                self.assertIsNotNone(linker, "native fixture requires CI's verified MSVC linker")
            if linker:
                selected = Path(linker)
                self.assertTrue(selected.is_absolute() and selected.is_file()
                                and selected.name.lower() == "link.exe")
                command.extend(["-C", f"linker={selected}"])
            compiled = subprocess.run([*command, str(source), "-o", str(executable)],
                                      capture_output=True, text=True, timeout=120)
            self.assertEqual(compiled.returncode, 0, compiled.stderr[-8192:])
            result = subprocess.run([str(executable)], capture_output=True,
                                    text=True, timeout=45)
            self.assertEqual(result.returncode, 0, (result.stdout + result.stderr)[-8192:])
            self.assertIn("Verified direct breakaway denial", result.stdout)


if __name__ == "__main__":
    unittest.main()
