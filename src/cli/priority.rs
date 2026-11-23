//! Scheduling priority for the CLI's long audits.
//!
//! `mdkb code dup --semantic` saturates every core for minutes and nothing is
//! waiting on it: it is a sweep someone asked for, not a query someone is
//! blocked on. Lowering this process changes no CPU-seconds — measured, the
//! pass costs what it costs — it changes who gets a core first when an editor,
//! a build and the audit all want one.
//!
//! Deliberately under `cli/`. The daemon serves MCP from a long-lived process
//! that answers interactive searches, so putting *it* in the background would
//! make every search pay for an audit nobody asked that process to run. The
//! only caller is the CLI's duplication entry point.

/// Put this process in the background scheduling class.
///
/// Returns whether the process is in that class afterwards. `false` on a
/// platform with no such class, and `false` if the kernel refused — neither is
/// something a caller should report, because an audit that ran at normal
/// priority is an audit that ran. That is why this returns a bool rather than a
/// `Result`: there is no failure here worth propagating, only a fact worth
/// asserting in a test.
///
/// macOS has a dedicated background *state* rather than a nice value, and it
/// throttles disk I/O along with CPU. For a sweep that reads the index once and
/// then spends minutes inside ONNX, that is the right trade. Elsewhere on unix
/// the lever is nice 19, the lowest an unprivileged process can ask for.
#[cfg(target_os = "macos")]
pub fn lower_to_background() -> bool {
    // SAFETY: both calls name this process — `who` = 0 is the only value macOS
    // accepts with PRIO_DARWIN_PROCESS — and neither reads nor writes memory.
    // A failure leaves the priority unchanged, which the read below reports.
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
        // getpriority answers 0 or 1 for this class, never a nice value, so
        // -1 is unambiguously an error and needs no errno dance.
        libc::getpriority(libc::PRIO_DARWIN_PROCESS, 0) == 1
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn lower_to_background() -> bool {
    // SAFETY: as above — this process, no memory touched.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::getpriority(libc::PRIO_PROCESS, 0) >= 10
    }
}

#[cfg(not(unix))]
pub fn lower_to_background() -> bool {
    false
}

/// Restore the normal scheduling class.
///
/// Exists for the tests: a unit test that backgrounds the process would leave
/// every test after it throttled. On macOS this is exact. On other unix it is
/// best-effort and will not succeed for an unprivileged process, which is why
/// the test there asserts what it can and says so.
#[cfg(all(test, target_os = "macos"))]
fn restore_normal() {
    // SAFETY: same call, same process; 0 is the documented "leave background".
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, 0);
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
fn restore_normal() {
    // SAFETY: same call, same process. An unprivileged process cannot raise its
    // own priority, so this is best effort; the tests below say so.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 0);
    }
}

#[cfg(all(test, not(unix)))]
fn restore_normal() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests mutate process-wide state, so they must not overlap: one
    /// restoring while the other reads would fail for a reason that has nothing
    /// to do with the code under test.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores the class even if the assertions panic, so a failure here
    /// cannot leave every later test in this binary running throttled.
    struct Restore;

    impl Drop for Restore {
        fn drop(&mut self) {
            restore_normal();
        }
    }

    /// The drop is real and asking twice changes nothing.
    ///
    /// Idempotence matters because the caller cannot know whether something
    /// already lowered the process — a second call must not stack into
    /// something the kernel refuses, or into something lower still.
    #[cfg(unix)]
    #[test]
    fn backgrounding_is_readable_afterwards_and_asking_twice_changes_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = Restore;

        let first = lower_to_background();
        let second = lower_to_background();

        assert!(first, "the process must be in the background class");
        assert_eq!(second, first, "a second call must be a no-op");
    }

    /// Nothing else in the suite inherits the throttle.
    ///
    /// The test above shares a process with every other test in this binary. If
    /// the restore did not work, the rest of the suite would run with throttled
    /// disk I/O, and the slowdown would surface as a flake somewhere unrelated.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_process_comes_back_out_of_the_background_class() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        lower_to_background();
        restore_normal();

        // SAFETY: reads this process's own class; no memory touched.
        let state = unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, 0) };

        assert_eq!(state, 0, "the process must be back at normal priority");
    }
}
