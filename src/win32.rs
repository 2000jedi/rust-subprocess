#![allow(non_snake_case, non_camel_case_types)]

use std::io::{Result, Error};
use std::fs::File;

use std::os::windows::io::{RawHandle, FromRawHandle, AsRawHandle};
use std::ptr;
use std::mem;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use kernel32;

use winapi;
use winapi::minwindef::{BOOL, DWORD};
use winapi::minwinbase::{SECURITY_ATTRIBUTES, LPSECURITY_ATTRIBUTES};
use winapi::processthreadsapi::*;
use winapi::winnt::PHANDLE;

pub use winapi::winerror::ERROR_BAD_PATHNAME;

//use common::StandardStream;

#[derive(Debug)]
pub struct Handle(RawHandle);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { kernel32::CloseHandle(self.as_raw_handle()); }
    }
}

impl AsRawHandle for Handle {
    fn as_raw_handle(&self) -> RawHandle {
        self.0
    }
}

impl FromRawHandle for Handle {
    unsafe fn from_raw_handle(handle: RawHandle) -> Handle {
        Handle(handle)
    }
}

pub const HANDLE_FLAG_INHERIT: u32 = 1;
pub const STARTF_USESTDHANDLES: DWORD = winapi::winbase::STARTF_USESTDHANDLES;

fn check(status: BOOL) -> Result<()> {
    if status != 0 {
        Ok(())
    } else {
        Err(Error::last_os_error())
    }
}

// OsStr to zero-terminated owned vector
fn to_nullterm(s: &OsStr) -> Vec<u16> {
    let mut vec: Vec<_> = s.encode_wide().collect();
    vec.push(0u16);
    vec
}

pub fn CreatePipe(inherit_handle: bool) -> Result<(File, File)> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: if inherit_handle { 1 } else { 0 },
    };
    let (mut r, mut w) = (ptr::null_mut(), ptr::null_mut()); 
    check(unsafe {
        kernel32::CreatePipe(&mut r as PHANDLE, &mut w as PHANDLE,
                             &mut attributes as LPSECURITY_ATTRIBUTES, 0)
    })?;
    Ok(unsafe { (File::from_raw_handle(r), File::from_raw_handle(w)) })
}
 
pub fn SetHandleInformation(handle: &mut File, dwMask: u32, dwFlags: u32) -> Result<()> {
    check(unsafe {
        kernel32::SetHandleInformation(handle.as_raw_handle(), dwMask, dwFlags)
    })?;
    Ok(())
}

fn handle_of(opt_handle: &Option<File>) -> RawHandle {
    match opt_handle.as_ref() {
        Some(ref handle) => handle.as_raw_handle(),
        None => ptr::null_mut()
    }
}

pub fn CreateProcess(cmdline: &OsStr,
                     inherit_handles: bool,
                     creation_flags: u32,
                     stdin: Option<File>,
                     stdout: Option<File>,
                     stderr: Option<File>,
                     sinfo_flags: u32) -> Result<(Handle, u64)> {
    let mut sinfo: STARTUPINFOW = unsafe { mem::zeroed() };
    sinfo.cb = mem::size_of::<STARTUPINFOW>() as DWORD;
    sinfo.hStdInput = handle_of(&stdin);
    sinfo.hStdOutput = handle_of(&stdout);
    sinfo.hStdError = handle_of(&stderr);
    sinfo.dwFlags = sinfo_flags;
    let mut pinfo: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    let mut cmdline = to_nullterm(OsStr::new(cmdline));
    check(unsafe {
        kernel32::CreateProcessW(ptr::null_mut(),
                                 &mut cmdline[0] as winapi::LPWSTR,
                                 ptr::null_mut(),   // lpProcessAttributes
                                 ptr::null_mut(),   // lpThreadAttributes
                                 if inherit_handles
                                 { 1 } else { 0 },  // bInheritHandles
                                 creation_flags,    // dwCreationFlags
                                 ptr::null_mut(),   // lpEnvironment
                                 ptr::null_mut(),   // lpCurrentDirectory
                                 &mut sinfo as LPSTARTUPINFOW,
                                 &mut pinfo as LPPROCESS_INFORMATION)
    })?;
    unsafe {
        mem::drop(Handle::from_raw_handle(pinfo.hThread));
        Ok((Handle::from_raw_handle(pinfo.hProcess), pinfo.dwProcessId as u64))
    }
}

pub enum WaitEvent {
    OBJECT_0,
    ABANDONED,
    TIMEOUT,
}

pub fn WaitForSingleObject(handle: &Handle, duration: Option<u32>)
                           -> Result<WaitEvent> {
    const WAIT_ABANDONED: u32 = 0x80;
    const WAIT_OBJECT_0: u32 = 0x0;
    const WAIT_FAILED: u32 = 0xFFFFFFFF;
    const WAIT_TIMEOUT: u32 = 0x102;
    const INFINITE: u32 = 0xFFFFFFFF;

    let result = unsafe {
        kernel32::WaitForSingleObject(handle.as_raw_handle(),
                                      duration.unwrap_or(INFINITE))
    };
    if result == WAIT_OBJECT_0 { Ok(WaitEvent::OBJECT_0) }
    else if result == WAIT_ABANDONED { Ok(WaitEvent::ABANDONED) }
    else if result == WAIT_TIMEOUT { Ok(WaitEvent::TIMEOUT) }
    else if result == WAIT_FAILED { Err(Error::last_os_error()) }
    else {
        panic!(format!("WaitForSingleObject returned {}", result));
    }
}

pub fn GetExitCodeProcess(handle: &Handle) -> Result<u32> {
    let mut exit_code = 0u32;
    check(unsafe {
        kernel32::GetExitCodeProcess(handle.as_raw_handle(),
                                     &mut exit_code as *mut u32)
    })?;
    Ok(exit_code)
}

// pub fn GetStdHandle(which: StandardStream) -> Result<File> {
//     use winapi::winbase::{STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE};
//     let id = match which {
//         StandardStream::Input => STD_INPUT_HANDLE,
//         StandardStream::Output => STD_OUTPUT_HANDLE,
//         StandardStream::Error => STD_ERROR_HANDLE,
//     };
//     let raw_handle = unsafe { kernel32::GetStdHandle(id) };
//     if raw_handle == winapi::INVALID_HANDLE_VALUE {
//         return Err(Error::last_os_error());
//     }
//     Ok(unsafe { File::from_raw_handle(raw_handle) })
// }

// pub fn get_standard_stream(which: StandardStream) -> Result<File> {
//     let f = GetStdHandle(which)?;
//     let cloned = f.try_clone()?;
//     mem::forget(f);
//     Ok(cloned)
// }

pub fn TerminateProcess(handle: &Handle, exit_code: u32) -> Result<()> {
    check(unsafe {
        kernel32::TerminateProcess(handle.as_raw_handle(),
                                   exit_code)
    })
}
