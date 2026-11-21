//! Keeps luau-lsp from outliving the proxy.
//!
//! Two things conspire to leave a server running once the editor is gone.
//! An editor that is closed or crashes does not always send `shutdown` and
//! `exit`, and luau-lsp does not stop when its input reaches EOF. And the
//! server is rarely the proxy's direct child: version managers put one or two
//! shims in between (mise, rokit), so ending the child the proxy spawned leaves
//! the real server running underneath it.
//!
//! Everything here therefore acts on the whole process tree below the proxy,
//! never on the direct child alone.

use std::process::{Child, Command};

/// Sets up what ends the tree, before any child exists. Call once.
///
/// On Windows the proxy puts itself in a job object that kills every process
/// in it when its last handle closes. Children inherit the job, so the tree
/// dies with the proxy however the proxy ends, killed included.
///
/// On Unix the server gets its own process group (see [`prepare`]), and
/// SIGTERM, SIGHUP and SIGINT kill that group before the proxy exits. SIGKILL
/// cannot be caught: on Linux the direct child at least gets SIGKILL too,
/// through `PR_SET_PDEATHSIG`.
pub fn guard_tree() -> Result<(), String> {
    imp::guard_tree()
}

/// Prepares the command that launches the server.
pub fn prepare(command: &mut Command) {
    imp::prepare(command);
}

/// Records the spawned server, so that the tree can be ended later.
pub fn adopt(child: &Child) {
    imp::adopt(child);
}

/// Ends the server and everything it launched. Harmless if they are gone.
///
/// On Windows, what survives the direct child is ended when the proxy exits
/// and the job closes, so this must be followed by the proxy's own exit.
pub fn end_tree(child: &mut Child) {
    imp::end_tree(child);
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::process::{Child, Command};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn guard_tree() -> Result<(), String> {
        let last_error = || std::io::Error::last_os_error().to_string();

        // SAFETY: plain Win32 calls on a handle created here. The handle is
        // not inheritable (null security attributes) and is never closed: the
        // system closes it when the proxy exits, which is what ends the job.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!("CreateJobObjectW: {}", last_error()));
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                return Err(format!("SetInformationJobObject: {}", last_error()));
            }

            // The proxy itself joins the job rather than its child: a child
            // assigned after spawning could launch the real server before the
            // assignment, and that server would escape the job.
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                return Err(format!("AssignProcessToJobObject: {}", last_error()));
            }
        }
        Ok(())
    }

    pub fn prepare(_command: &mut Command) {}

    pub fn adopt(_child: &Child) {}

    pub fn end_tree(child: &mut Child) {
        // Covers the direct child even if joining the job failed.
        let _ = child.kill();
    }
}

#[cfg(unix)]
mod imp {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicI32, Ordering};

    /// The server's process group, 0 until it is spawned.
    static GROUP: AtomicI32 = AtomicI32::new(0);

    fn kill_group() {
        let group = GROUP.load(Ordering::SeqCst);
        if group > 0 {
            // SAFETY: kill is async-signal-safe and takes plain integers.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        kill_group();
        // SAFETY: _exit is async-signal-safe, unlike std::process::exit.
        unsafe { libc::_exit(128 + signal) }
    }

    pub fn guard_tree() -> Result<(), String> {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            // SAFETY: the handler only calls async-signal-safe functions.
            let previous = unsafe { libc::signal(signal, handler) };
            if previous == libc::SIG_ERR {
                return Err(format!(
                    "signal({signal}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    pub fn prepare(command: &mut Command) {
        // A group of its own, led by the direct child: whatever the shims
        // launch stays in it, so one kill reaches the real server.
        command.process_group(0);

        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe, as pre_exec requires. The
        // signal follows the death of the thread that spawned the child: that
        // is the main thread, which lives as long as the proxy.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    pub fn adopt(child: &Child) {
        GROUP.store(child.id() as i32, Ordering::SeqCst);
    }

    pub fn end_tree(child: &mut Child) {
        kill_group();
        let _ = child.kill();
    }
}
