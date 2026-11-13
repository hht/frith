//! A child process on a pseudo-terminal.
//!
//! Answers: how does a program get started on a PTY, resized, and ended?
//! Must not know: terminal emulation, what the bytes mean, or anything on screen.
//!
//! The child runs in its own session with the PTY as controlling terminal, as in
//! Alacritty. Ending it sends SIGHUP to its whole process group and reaps it on a
//! background thread, so closing a pane never blocks the main thread or leaves a zombie.
//! @see https://github.com/alacritty/alacritty/blob/v0.15.1/alacritty_terminal/src/tty/unix.rs

use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{ptr, thread};

/// Held from `openpty` until both fds are close-on-exec, and across `fork`, so no child
/// spawned on another thread inherits a PTY fd in between. macOS has no atomic close-on-exec
/// for PTYs: `posix_openpt` accepts only `O_RDWR` and `O_NOCTTY` (posix_openpt(2)). Any other
/// code that spawns processes must hold it too.
static SPAWN: Mutex<()> = Mutex::new(());

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Program {
    /// The user's login shell, interactive.
    LoginShell,
    /// A command line run by the user's login shell (`$SHELL -l -c`), so it sees the
    /// user's PATH.
    Command(String),
}

/// Grid and pixel size reported to the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Size {
    pub cols: u16,
    pub rows: u16,
    pub width: u16,
    pub height: u16,
}

pub(crate) struct Pty {
    master: File,
    /// `Some` until drop hands it to the reaper thread.
    child: Option<Child>,
}

impl Pty {
    /// Starts `program` on a new PTY of `size` in `cwd`, with `env` added to this process's
    /// environment.
    pub fn spawn(program: &Program, size: Size, cwd: &Path, env: &[(&str, &OsStr)]) -> io::Result<Pty> {
        let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = (-1, -1);
        let mut ws = winsize(size);
        // SAFETY: openpty writes two fds into the out-pointers and reads `ws`.
        if unsafe { libc::openpty(&mut master, &mut slave, ptr::null_mut(), ptr::null_mut(), &mut ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty succeeded, so both fds are open and owned by nobody else.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // The child gets the slave as stdio (dup2 clears close-on-exec there); no other
        // program may inherit either end.
        set_cloexec(&master)?;
        set_cloexec(&slave)?;

        let shell = login_shell()?;
        let name = shell.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut cmd = Command::new(&shell);
        cmd.arg0(format!("-{name}")); // a leading '-' in argv[0] makes it a login shell
        if let Program::Command(line) = program {
            cmd.arg("-c").arg(line);
        }
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?))
            .current_dir(cwd)
            .envs(env.iter().copied());
        // SAFETY: only async-signal-safe calls between fork and exec. std has already
        // dup2'ed the slave onto fds 0-2 when pre_exec closures run.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Ignored signals survive exec; a child that inherits an ignored SIGHUP
                // would outlive its pane. Alacritty resets the same set.
                for sig in [libc::SIGCHLD, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGALRM] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave); // so reads see EOF once the child and its descendants close it
        Ok(Pty { master: File::from(master), child: Some(child) })
    }

    /// A handle that reads the child's output until it exits.
    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    /// A handle that writes input to the child.
    pub fn writer(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn resize(&self, size: Size) -> io::Result<()> {
        let ws = winsize(size);
        // SAFETY: TIOCSWINSZ reads one winsize from the pointer.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Pty {
    /// Hangs up the child's whole process group, as closing a terminal window does, and
    /// reaps the child off the main thread. A hangup that lands while a shell is between fork
    /// and exec can be lost (seen with zsh, for the shell and for a job it just started). So
    /// if the child (the group leader) is still alive after `HANGUP_GRACE`, the whole group
    /// gets SIGKILL, since some programs also ignore SIGHUP. Once the leader is gone, the group
    /// is hung up again every 10 ms until no live member is left or the grace ends; members
    /// that ignore SIGHUP then live on, as `nohup` intends. Ghostty instead repeats its hangup
    /// while the leader lives and stops once the leader is reaped, which leaves running a job
    /// that missed every hangup sent while the leader lived.
    /// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/termio/Exec.zig (`killPid`)
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        // The child called setsid, so its pid is its process group id.
        let group = child.id() as libc::pid_t;
        hang_up(group);
        thread::spawn(move || {
            let deadline = Instant::now() + HANGUP_GRACE;
            while Instant::now() < deadline {
                if !matches!(child.try_wait(), Ok(None)) && !hang_up(group) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if matches!(child.try_wait(), Ok(None)) {
                // SAFETY: plain syscall; a group that already exited yields ESRCH, which is fine.
                unsafe { libc::killpg(group, libc::SIGKILL) };
                let _ = child.wait();
            }
        });
    }
}

/// Sends SIGHUP to `group`; false once no member can get it: killpg(2) fails with ESRCH when
/// the group is empty, and on macOS with EPERM when only zombies are left.
fn hang_up(group: libc::pid_t) -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::killpg(group, libc::SIGHUP) == 0 }
}

/// How long a hung-up child may take to exit before it is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(1);

fn winsize(s: Size) -> libc::winsize {
    libc::winsize { ws_row: s.rows, ws_col: s.cols, ws_xpixel: s.width, ws_ypixel: s.height }
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain fcntl on an open fd.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The user's shell from the password database, which is set even when the app was started
/// from Finder without a `SHELL` variable.
pub(crate) fn login_shell() -> io::Result<PathBuf> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out = ptr::null_mut();
    // SAFETY: getpwuid_r fills `pw` with pointers into `buf`, which outlives their use here.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pw.pw_shell.is_null() {
        return Err(io::Error::other("no password database entry for the current user"));
    }
    // SAFETY: pw_shell points to a NUL-terminated string inside `buf`.
    let shell = unsafe { CStr::from_ptr(pw.pw_shell) };
    Ok(PathBuf::from(OsStr::from_bytes(shell.to_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const SIZE: Size = Size { cols: 80, rows: 24, width: 800, height: 480 };

    fn spawn(line: &str) -> Pty {
        Pty::spawn(&Program::Command(line.into()), SIZE, Path::new("/"), &[]).unwrap()
    }

    fn output_of(pty: &Pty) -> String {
        let mut out = Vec::new();
        let _ = pty.reader().unwrap().read_to_end(&mut out); // EIO at EOF on macOS
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn a_resize_reaches_the_child() {
        let pty = spawn("sleep 0.3; stty size");
        pty.resize(Size { cols: 100, rows: 30, width: 1000, height: 600 }).unwrap();
        let out = output_of(&pty);
        assert!(out.contains("30 100"), "{out:?}");
    }

    /// Starts `line`, whose first line of output is a background job's pid and which ends with
    /// `exec sleep`, reads the pid, and waits until the shell has become that `sleep`, so the
    /// shell's own hangup cannot be lost.
    fn spawn_with_job(line: &str) -> (Pty, libc::pid_t) {
        let pty = spawn(line);
        let mut reader = pty.reader().unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8];
        while byte[0] != b'\n' {
            reader.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&line);
        let job = text.trim().parse().unwrap_or_else(|e| panic!("{text:?}: {e}"));
        let shell = pty.child.as_ref().unwrap().id().to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        let comm = || {
            let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            Command::new("ps").args(["-o", "comm=", "-p", &shell]).output().unwrap().stdout
        };
        while comm() != b"sleep\n" {
            assert!(Instant::now() < deadline, "the shell never ran its last command");
            thread::sleep(Duration::from_millis(5));
        }
        (pty, job)
    }

    /// A zombie still counts as present.
    fn gone(pid: libc::pid_t) -> bool {
        (unsafe { libc::kill(pid, 0) }) != 0
    }

    #[test]
    fn dropping_ends_the_whole_process_group_and_reaps_the_child() {
        let (pty, background) = spawn_with_job("sleep 100 & echo $! ; exec sleep 100");
        let shell = pty.child.as_ref().unwrap().id() as libc::pid_t;
        drop(pty);
        let deadline = Instant::now() + HANGUP_GRACE + Duration::from_secs(1);
        while !(gone(shell) && gone(background)) {
            assert!(Instant::now() < deadline, "shell or background job survived");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Starts `line` as `spawn_with_job` does, closes it, and tells whether the job is still
    /// alive once the hangup grace is over; the job is killed either way.
    fn job_outlives_its_pane(line: &str) -> bool {
        let (pty, job) = spawn_with_job(line);
        drop(pty);
        thread::sleep(HANGUP_GRACE + Duration::from_millis(200));
        let alive = !gone(job);
        unsafe { libc::kill(job, libc::SIGKILL) };
        alive
    }

    #[test]
    fn a_job_that_missed_the_first_hangup_still_ends() {
        // Deaf to the hangup sent at drop, the job listens again before the grace ends.
        let line = "sh -c 'trap \"\" HUP; echo $$; sleep 0.3; trap - HUP; exec sleep 100' & exec sleep 100";
        assert!(!job_outlives_its_pane(line), "a job that missed the first hangup survived its pane");
    }

    #[test]
    fn a_job_that_ignores_hangups_outlives_its_pane() {
        // The job prints its pid only once it ignores SIGHUP, and keeps that pid across exec.
        let line = "sh -c 'trap \"\" HUP; echo $$; exec sleep 100' & exec sleep 100";
        assert!(job_outlives_its_pane(line), "a job that ignores SIGHUP was killed");
    }

    #[test]
    fn a_command_runs_on_the_pty_and_sees_its_size() {
        let pty = spawn("stty size; printf hello");
        let out = output_of(&pty);
        assert!(out.contains("24 80"), "{out:?}");
        assert!(out.contains("hello"), "{out:?}");
    }
}
