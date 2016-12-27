extern crate tempdir;

use super::super::{Popen, ExitStatus, Redirection};
use std::fs::File;
use std::io::Write;
use std::mem;
use libc::SIGTERM;

use self::tempdir::TempDir;

use tests::common::read_whole_file;

#[test]
fn err_terminate() {
    let mut p = Popen::create(&["sleep", "5"]).unwrap();
    assert!(p.poll().is_none());
    p.terminate().unwrap();
    assert!(p.wait().unwrap() == ExitStatus::Signaled(SIGTERM as u8));
}

#[test]
fn write_to_subprocess() {
    let tmpdir = TempDir::new("test").unwrap();
    let tmpname = tmpdir.path().join("output");
    let mut p = Popen::create_full(
        &["dd".to_string(), format!("of={}", tmpname.display()), "status=none".to_string()],
        Redirection::Pipe, Redirection::None, Redirection::None)
        .unwrap();
    p.stdin.as_mut().unwrap().write_all(b"foo").unwrap();
    mem::drop(p.stdin.take());
    assert!(p.wait().unwrap() == ExitStatus::Exited(0));
    assert!(read_whole_file(File::open(tmpname).unwrap()) == "foo");
}
