extern crate crossbeam;

use std::result;
use std::error::Error;
use std::io;
use std::io::{Read, Write, Result as IoResult};
use std::fs::File;
use std::string::FromUtf8Error;
use std::fmt;
use std::ffi::{OsStr, OsString};
use std::time::Duration;

use common::{ExitStatus, StandardStream};

#[derive(Debug)]
pub struct Popen {
    _pid: Option<u32>,
    exit_status: Option<ExitStatus>,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,

    ext_data: os::ExtPopenData,
}

#[derive(Debug)]
pub enum Redirection {
    None,
    File(File),
    Pipe,
    Merge,
}

impl Default for Redirection {
    fn default() -> Redirection {
        Redirection::None
    }
}

#[derive(Debug)]
pub struct PopenConfig {
    // Force construction using ..Default::default(), so we can add
    // new public fields without breaking code
    pub _use_default_to_construct: (),

    pub stdin: Redirection,
    pub stdout: Redirection,
    pub stderr: Redirection,

    // executable, cwd, env, preexec_fn, close_fds...
}

impl Default for PopenConfig {
    fn default() -> PopenConfig {
        PopenConfig {
            _use_default_to_construct: (),
            stdin: Redirection::None,
            stdout: Redirection::None,
            stderr: Redirection::None,
        }
    }
}

impl Popen {
    pub fn create<S: AsRef<OsStr>>(argv: &[S], config: PopenConfig)
        -> Result<Popen>
    {
        let argv: Vec<OsString> = argv.iter()
            .map(|p| p.as_ref().to_owned()).collect();
        let mut inst = Popen {
            _pid: None,
            exit_status: None,
            stdin: None,
            stdout: None,
            stderr: None,
            ext_data: os::ExtPopenData::default(),
        };
        inst.start(argv, config.stdin, config.stdout, config.stderr)?;
        Ok(inst)
    }

    pub fn detach(&mut self) {
        self._pid = None;
    }

    fn make_child_streams(&mut self, stdin: Redirection, stdout: Redirection, stderr: Redirection)
                          -> Result<(Option<File>, Option<File>, Option<File>)> {
        fn prepare_pipe(for_write: bool, store_parent_end: &mut Option<File>) -> Result<File> {
            let (read, write) = os::make_pipe()?;
            let (mut parent_end, child_end) =
                if for_write {(write, read)} else {(read, write)};
            os::set_inheritable(&mut parent_end, false)?;
            *store_parent_end = Some(parent_end);
            Ok(child_end)
        }
        fn prepare_file(mut file: File) -> IoResult<File> {
            os::set_inheritable(&mut file, true)?;
            Ok(file)
        }
        enum MergeKind {
            OutToErr, // 1>&2
            ErrToOut, // 2>&1
            None,
        }
        let mut merge: MergeKind = MergeKind::None;

        let child_stdin = match stdin {
            Redirection::Pipe => Some(prepare_pipe(true, &mut self.stdin)?),
            Redirection::File(file) => Some(prepare_file(file)?),
            Redirection::Merge => {
                return Err(PopenError::LogicError("Redirection::Merge not valid for stdin"));
            }
            Redirection::None => None,
        };
        let mut child_stdout = match stdout {
            Redirection::Pipe => Some(prepare_pipe(false, &mut self.stdout)?),
            Redirection::File(file) => Some(prepare_file(file)?),
            Redirection::Merge => { merge = MergeKind::ErrToOut; None },
            Redirection::None => None,
        };
        let mut child_stderr = match stderr {
            Redirection::Pipe => Some(prepare_pipe(false, &mut self.stderr)?),
            Redirection::File(file) => Some(prepare_file(file)?),
            Redirection::Merge => { merge = MergeKind::OutToErr; None },
            Redirection::None => None,
        };

        fn dup_child_stream(child_stream: &mut Option<File>, s: StandardStream) -> IoResult<File> {
            if child_stream.is_none() {
                *child_stream = Some(os::clone_standard_stream(s)?);
            }
            child_stream.as_ref().unwrap().try_clone()
        }

        match merge {
            MergeKind::OutToErr => child_stderr = Some(dup_child_stream(&mut child_stdout, StandardStream::Output)?),
            MergeKind::ErrToOut => child_stdout = Some(dup_child_stream(&mut child_stderr, StandardStream::Error)?),
            MergeKind::None => (),
        }

        Ok((child_stdin, child_stdout, child_stderr))
    }

    fn comm_read(outfile: &mut Option<File>) -> IoResult<Vec<u8>> {
        let mut contents = Vec::new();
        outfile.as_mut().expect("file missing").read_to_end(&mut contents)?;
        outfile.take();
        Ok(contents)
    }

    fn comm_write(infile: &mut Option<File>, input_data: &[u8]) -> IoResult<()> {
        infile.as_mut().expect("file missing").write_all(input_data)?;
        infile.take();
        Ok(())
    }

    pub fn communicate_bytes(&mut self, input_data: Option<&[u8]>)
                             -> IoResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        match (&mut self.stdin, &mut self.stdout, &mut self.stderr) {
            (mut stdin_ref @ &mut Some(_), &mut None, &mut None) => {
                let input_data = input_data.expect("must provide input to redirected stdin");
                Popen::comm_write(stdin_ref, input_data)?;
                Ok((None, None))
            }
            (&mut None, mut stdout_ref @ &mut Some(_), &mut None) => {
                assert!(input_data.is_none(), "cannot provide input to non-redirected stdin");
                let out = Popen::comm_read(stdout_ref)?;
                Ok((Some(out), None))
            }
            (&mut None, &mut None, mut stderr_ref @ &mut Some(_)) => {
                assert!(input_data.is_none(), "cannot provide input to non-redirected stdin");
                let err = Popen::comm_read(stderr_ref)?;
                Ok((None, Some(err)))
            }
            (ref mut stdin_ref, ref mut stdout_ref, ref mut stderr_ref) =>
                crossbeam::scope(move |scope| {
                    let (mut out_thr, mut err_thr) = (None, None);
                    if stdout_ref.is_some() {
                        out_thr = Some(scope.spawn(move || Popen::comm_read(stdout_ref)))
                    }
                    if stderr_ref.is_some() {
                        err_thr = Some(scope.spawn(move || Popen::comm_read(stderr_ref)))
                    }
                    if stdin_ref.is_some() {
                        let input_data = input_data.expect("must provide input to redirected stdin");
                        Popen::comm_write(stdin_ref, input_data)?;
                    }
                    Ok((if let Some(out_thr) = out_thr {Some(out_thr.join()?)} else {None},
                        if let Some(err_thr) = err_thr {Some(err_thr.join()?)} else {None}))
                })
        }
    }

    pub fn communicate(&mut self, input_data: Option<&str>)
                       -> Result<(Option<String>, Option<String>)> {
        let (out, err) = self.communicate_bytes(input_data.map(|s| s.as_bytes()))?;
        let out_str = if let Some(out_vec) = out {
            Some(String::from_utf8(out_vec)?)
        } else { None };
        let err_str = if let Some(err_vec) = err {
            Some(String::from_utf8(err_vec)?)
        } else { None };
        Ok((out_str, err_str))
    }

    pub fn pid(&self) -> Option<u32> {
        self._pid
    }

    fn start(&mut self,
             argv: Vec<OsString>,
             stdin: Redirection, stdout: Redirection, stderr: Redirection)
             -> Result<()> {
        (self as &mut PopenOs).start(argv, stdin, stdout, stderr)
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        (self as &mut PopenOs).wait()
    }

    pub fn wait_timeout(&mut self, dur: Duration) -> Result<Option<ExitStatus>> {
        (self as &mut PopenOs).wait_timeout(dur)
    }

    pub fn poll(&mut self) -> Option<ExitStatus> {
        (self as &mut PopenOs).poll()
    }

    pub fn terminate(&mut self) -> IoResult<()> {
        (self as &mut PopenOs).terminate()
    }

    pub fn kill(&mut self) -> IoResult<()> {
        (self as &mut PopenOs).kill()
    }
}


trait PopenOs {
    fn start(&mut self, argv: Vec<OsString>,
             stdin: Redirection, stdout: Redirection, stderr: Redirection)
             -> Result<()>;
    fn wait(&mut self) -> Result<ExitStatus>;
    fn wait_timeout(&mut self, dur: Duration) -> Result<Option<ExitStatus>>;
    fn poll(&mut self) -> Option<ExitStatus>;
    fn terminate(&mut self) -> IoResult<()>;
    fn kill(&mut self) -> IoResult<()>;

}

#[cfg(unix)]
mod os {
    use super::*;
    use std::io;
    use std::io::{Read, Write, Result as IoResult};
    use std::fs::File;
    use posix;
    use std::mem;
    use std::os::unix::io::AsRawFd;
    use common::ExitStatus;
    use std::ffi::OsString;
    use std::time::Duration;

    pub type ExtPopenData = ();

    impl super::PopenOs for Popen {
        fn start(&mut self,
                 argv: Vec<OsString>,
                 stdin: Redirection, stdout: Redirection, stderr: Redirection)
                 -> Result<()> {
            let mut exec_fail_pipe = posix::pipe()?;
            set_inheritable(&mut exec_fail_pipe.0, false)?;
            set_inheritable(&mut exec_fail_pipe.1, false)?;
            {
                let child_ends = self.make_child_streams(stdin, stdout, stderr)?;
                let child_pid = posix::fork()?;
                if child_pid == 0 {
                    mem::drop(exec_fail_pipe.0);
                    let result: IoResult<()> = self.do_exec(argv, child_ends);
                    // Notify the parent process that exec has failed, and exit.
                    let error_code: i32 = match result {
                        Ok(()) => unreachable!(),
                        Err(e) => e.raw_os_error().unwrap_or(-1)
                    };
                    // XXX use the byteorder crate to serialize the error
                    exec_fail_pipe.1.write_all(format!("{}", error_code).as_bytes())
                        .expect("write to error pipe");
                    posix::_exit(127);
                }
                self._pid = Some(child_pid as u32);
            }
            mem::drop(exec_fail_pipe.1);
            let mut error_string = String::new();
            exec_fail_pipe.0.read_to_string(&mut error_string)?;
            if error_string.len() != 0 {
                let error_code: i32 = error_string.parse()
                    .expect("parse child error code");
                Err(PopenError::from(io::Error::from_raw_os_error(error_code)))
            } else {
                Ok(())
            }
        }

        fn wait(&mut self) -> Result<ExitStatus> {
            while let None = self.exit_status {
                self.waitpid(0)?;
            }
            Ok(self.exit_status.unwrap())
        }

        fn wait_timeout(&mut self, dur: Duration) -> Result<Option<ExitStatus>> {
            use std::cmp::min;
            use std::time::{Instant, Duration};
            use std::thread;

            if self.exit_status.is_some() {
                return Ok(self.exit_status);
            }

            let deadline = Instant::now() + dur;
            // delay doubles at every iteration, so initial delay will be 1ms
            let mut delay = Duration::new(0, 500_000);
            loop {
                self.waitpid(posix::WNOHANG)?;
                if self.exit_status.is_some() {
                    return Ok(self.exit_status);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                let remaining = deadline.duration_since(now);
                delay = min(delay * 2, min(remaining, Duration::from_millis(100)));
                thread::sleep(delay);
            }
        }

        fn poll(&mut self) -> Option<ExitStatus> {
            match self.waitpid(posix::WNOHANG) {
                Ok(_) => self.exit_status,
                Err(_) => None
            }
        }

        fn terminate(&mut self) -> IoResult<()> {
            self.send_signal(posix::SIGTERM)
        }

        fn kill(&mut self) -> IoResult<()> {
            self.send_signal(posix::SIGKILL)
        }
    }

    trait PopenOsImpl: super::PopenOs {
        fn do_exec(&self, argv: Vec<OsString>,
                   child_ends: (Option<File>, Option<File>, Option<File>)) -> IoResult<()>;
        fn waitpid(&mut self, flags: i32) -> IoResult<()>;
        fn send_signal(&self, signal: u8) -> IoResult<()>;
    }

    impl PopenOsImpl for Popen {
        fn do_exec(&self, argv: Vec<OsString>,
                   child_ends: (Option<File>, Option<File>, Option<File>)) -> IoResult<()> {
            let (stdin, stdout, stderr) = child_ends;
            if let Some(stdin) = stdin {
                posix::dup2(stdin.as_raw_fd(), 0)?;
            }
            if let Some(stdout) = stdout {
                posix::dup2(stdout.as_raw_fd(), 1)?;
            }
            if let Some(stderr) = stderr {
                posix::dup2(stderr.as_raw_fd(), 2)?;
            }
            posix::execvp(&argv[0], &argv)
        }

        fn waitpid(&mut self, flags: i32) -> IoResult<()> {
            match self._pid {
                Some(pid) => {
                    // XXX handle some kinds of error - at least ECHILD and EINTR
                    let (pid_out, exit_status) = posix::waitpid(pid, flags)?;
                    if pid_out == pid {
                        self._pid = None;
                        self.exit_status = Some(exit_status);
                    }
                },
                None => (),
            }
            Ok(())
        }

        fn send_signal(&self, signal: u8) -> IoResult<()> {
            match self._pid {
                Some(pid) => {
                    posix::kill(pid, signal)
                },
                None => Ok(()),
            }
        }
    }

    pub fn set_inheritable(f: &mut File, inheritable: bool) -> IoResult<()> {
        if inheritable {
            // Unix pipes are inheritable by default.
        } else {
            let fd = f.as_raw_fd();
            let old = posix::fcntl(fd, posix::F_GETFD, None)?;
            posix::fcntl(fd, posix::F_SETFD, Some(old | posix::FD_CLOEXEC))?;
        }
        Ok(())
    }

    pub fn make_pipe() -> IoResult<(File, File)> {
        posix::pipe()
    }

    pub use posix::clone_standard_stream;
}


#[cfg(windows)]
mod os {
    use super::*;
    use std::io;
    use std::fs::File;
    use win32;
    use common::{ExitStatus, StandardStream};
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::time::Duration;
    use std::io::Result as IoResult;

    #[derive(Debug, Default)]
    pub struct ExtPopenData {
        handle: Option<win32::Handle>,
    }

    impl super::PopenOs for Popen {
        fn start(&mut self,
                 argv: Vec<OsString>,
                 stdin: Redirection, stdout: Redirection, stderr: Redirection)
                 -> Result<()> {
            let (mut child_stdin, mut child_stdout, mut child_stderr)
                = self.make_child_streams(stdin, stdout, stderr)?;
            ensure_child_stream(&mut child_stdin, StandardStream::Input)?;
            ensure_child_stream(&mut child_stdout, StandardStream::Output)?;
            ensure_child_stream(&mut child_stderr, StandardStream::Error)?;
            let cmdline = assemble_cmdline(argv)?;
            let (handle, pid)
                = win32::CreateProcess(&cmdline, true, 0,
                                       child_stdin, child_stdout, child_stderr,
                                       win32::STARTF_USESTDHANDLES)?;
            self._pid = Some(pid as u32);
            self.ext_data.handle = Some(handle);
            Ok(())
        }

        fn wait(&mut self) -> Result<ExitStatus> {
            self.wait_handle(None)?;
            match self.exit_status {
                Some(exit_status) => Ok(exit_status),
                // Since we invoked wait_handle without timeout, exit status should
                // exist at this point.  The only way for it not to exist would be if
                // something strange happened, like WaitForSingleObject returneing
                // something other than OBJECT_0.
                None => Err(PopenError::LogicError("Failed to obtain exit status"))
            }
        }

        fn wait_timeout(&mut self, dur: Duration) -> Result<Option<ExitStatus>> {
            if self.exit_status.is_some() {
                return Ok(self.exit_status);
            }
            self.wait_handle(Some(dur.as_secs() as f64
                                  + dur.subsec_nanos() as f64 * 1e-9))?;
            Ok(self.exit_status)
        }

        fn poll(&mut self) -> Option<ExitStatus> {
            match self.wait_handle(Some(0.0)) {
                Ok(_) => self.exit_status,
                Err(_) => None
            }
        }

        fn terminate(&mut self) -> IoResult<()> {
            if self.ext_data.handle.is_some() {
                match win32::TerminateProcess(self.ext_data.handle.as_ref().unwrap(), 1) {
                    Err(err) => {
                        if err.raw_os_error() != Some(win32::ERROR_ACCESS_DENIED as i32) {
                            return Err(err);
                        }
                        let rc = win32::GetExitCodeProcess(self.ext_data.handle.as_ref().unwrap())?;
                        if rc == win32::STILL_ACTIVE {
                            return Err(err);
                        }
                        self.exit_status = Some(ExitStatus::Exited(rc));
                        self._pid = None;
                        self.ext_data.handle = None;
                    }
                    Ok(_) => ()
                }
            }
            Ok(())
        }

        fn kill(&mut self) -> IoResult<()> {
            self.terminate()
        }
    }

    trait PopenOsImpl: super::PopenOs {
        fn wait_handle(&mut self, timeout: Option<f64>) -> IoResult<Option<ExitStatus>>;
    }

    impl PopenOsImpl for Popen {
        fn wait_handle(&mut self, timeout: Option<f64>) -> IoResult<Option<ExitStatus>> {
            if self.ext_data.handle.is_some() {
                let timeout = timeout.map(|t| (t * 1000.0) as u32);
                let event = win32::WaitForSingleObject(
                    self.ext_data.handle.as_ref().unwrap(), timeout)?;
                if let win32::WaitEvent::OBJECT_0 = event {
                    self._pid = None;
                    let handle = self.ext_data.handle.take().unwrap();
                    let exit_code = win32::GetExitCodeProcess(&handle)?;
                    self.exit_status = Some(ExitStatus::Exited(exit_code));
                }
            }
            Ok(self.exit_status)
        }
    }

    fn ensure_child_stream(stream: &mut Option<File>, which: StandardStream)
                           -> IoResult<()> {
        // If no stream is sent to CreateProcess, the child doesn't
        // get a valid stream.  This results in
        // Run("sh").arg("-c").arg("echo foo >&2").stream_stderr()
        // failing because the shell tries to redirect stdout to
        // stderr, but fails because it didn't receive a valid stdout.
        if stream.is_none() {
            *stream = Some(clone_standard_stream(which)?);
        }
        Ok(())
    }

    pub fn set_inheritable(f: &mut File, inheritable: bool) -> IoResult<()> {
        win32::SetHandleInformation(f, win32::HANDLE_FLAG_INHERIT,
                                         if inheritable {1} else {0})?;
        Ok(())
    }

    pub fn make_pipe() -> IoResult<(File, File)> {
        win32::CreatePipe(true)
    }

    fn assemble_cmdline(argv: Vec<OsString>) -> IoResult<OsString> {
        let mut cmdline = Vec::<u16>::new();
        for arg in argv {
            if arg.encode_wide().any(|c| c == 0) {
                return Err(io::Error::from_raw_os_error(win32::ERROR_BAD_PATHNAME as i32));
            }
            append_quoted(&arg, &mut cmdline);
            cmdline.push(' ' as u16);
        }
        Ok(OsString::from_wide(&cmdline))
    }

    // Translated from ArgvQuote at http://tinyurl.com/zmgtnls
    fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
        if !arg.is_empty() && !arg.encode_wide().any(
            |c| c == ' ' as u16 || c == '\t' as u16 || c == '\n' as u16 ||
                c == '\x0b' as u16 || c == '\"' as u16) {
            cmdline.extend(arg.encode_wide());
            return
        }
        cmdline.push('"' as u16);
        
        let arg: Vec<_> = arg.encode_wide().collect();
        let mut i = 0;
        while i < arg.len() {
            let mut num_backslashes = 0;
            while i < arg.len() && arg[i] == '\\' as u16 {
                i += 1;
                num_backslashes += 1;
            }
            
            if i == arg.len() {
                for _ in 0..num_backslashes*2 {
                    cmdline.push('\\' as u16);
                }
                break;
            } else if arg[i] == b'"' as u16 {
                for _ in 0..num_backslashes*2 + 1 {
                    cmdline.push('\\' as u16);
                }
                cmdline.push(arg[i]);
            } else {
                for _ in 0..num_backslashes {
                    cmdline.push('\\' as u16);
                }
                cmdline.push(arg[i]);
            }
            i += 1;
        }
        cmdline.push('"' as u16);
    }

    pub use win32::clone_standard_stream;
}


impl Drop for Popen {
    // Wait for the process to exit.  To avoid the wait, call
    // detach().
    fn drop(&mut self) {
        // drop() is invoked if a try! fails during construction, in which
        // case wait() would panic because an exit status cannot be obtained.
        if self.exit_status.is_none() {
            // XXX Log error occurred during wait()?
            self.wait().ok();
        }
    }
}


#[derive(Debug)]
pub enum PopenError {
    UtfError(FromUtf8Error),
    IoError(io::Error),
    LogicError(&'static str),
}

impl From<FromUtf8Error> for PopenError {
    fn from(err: FromUtf8Error) -> PopenError {
        PopenError::UtfError(err)
    }
}

impl From<io::Error> for PopenError {
    fn from(err: io::Error) -> PopenError {
        PopenError::IoError(err)
    }
}

impl Error for PopenError {
    fn description(&self) -> &str {
        match *self {
            PopenError::UtfError(ref err) => err.description(),
            PopenError::IoError(ref err) => err.description(),
            PopenError::LogicError(description) => description,
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            PopenError::UtfError(ref err) => Some(err as &Error),
            PopenError::IoError(ref err) => Some(err as &Error),
            PopenError::LogicError(_) => None,
        }
    }
}

impl fmt::Display for PopenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PopenError::UtfError(ref err) => fmt::Display::fmt(err, f),
            PopenError::IoError(ref err) => fmt::Display::fmt(err, f),
            PopenError::LogicError(desc) => f.write_str(desc)
        }
    }
}

pub type Result<T> = result::Result<T, PopenError>;
