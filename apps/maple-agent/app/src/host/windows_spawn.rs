//! Detach a host without overriding an enclosing Windows job's policy.

use std::io;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

const DETACHED_PROCESS: u32 = 0x00000008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
const ERROR_ACCESS_DENIED: i32 = 5;

pub(super) fn spawn(command: &mut Command) -> io::Result<Child> {
    let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match command
        .creation_flags(detached | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            // A job can refuse breakaway even when ordinary child creation is
            // allowed. Keep that job's restrictions and lifetime on the host;
            // retrying without breakaway never relaxes the enclosing policy.
            command.creation_flags(detached).spawn()
        }
        result => result,
    }
}
