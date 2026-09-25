//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Fatal signals in a process that loaded plugins.
//!
//! When the first plugin library loads, the host installs one handler for
//! `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE` and `SIGABRT`. On a crash it writes a
//! dump to stderr that lists every plugin library with its load address and
//! build id, so a stripped plugin can be symbolized offline. Then it runs the
//! handlers plugins registered through `MqbHostVTable::register_crash_handler`,
//! and finally hands the signal to whichever handler was installed before.
//!
//! `MQB_PLUGIN_CRASH_REPORT=0` turns the dump off; registered handlers still run.

use std::ffi::c_void;

use crate::support::plugin_abi::{MqbCrashHandler, MqbStatus};

pub(super) use imp::{install, record_library};

pub(super) unsafe extern "C" fn register_crash_handler(
    handler: MqbCrashHandler,
    user_data: *mut c_void,
) -> MqbStatus {
    imp::register(handler, user_data)
}

#[cfg(unix)]
mod imp {
    use std::ffi::{c_int, c_void, CStr};
    use std::fmt::Write as _;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::Once;

    use crate::support::plugin_abi::{
        MqbCrashHandler, MqbCrashInfo, MqbStatus, MQB_ERR_PERMANENT, MQB_OK,
    };

    const SIGNALS: [c_int; 5] = [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGABRT,
    ];
    const MAX_HANDLERS: usize = 32;
    const MAX_LIBRARIES: usize = 128;
    #[cfg(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu")))]
    const MAX_FRAMES: usize = 64;
    /// How long a second crashing thread waits for the first one's report.
    const MAX_WAIT_TICKS: u32 = 200;

    struct Registered {
        handler: unsafe extern "C" fn(*mut c_void, *const MqbCrashInfo),
        user_data: *mut c_void,
    }

    /// Rendered at load time, so the signal handler only copies bytes.
    struct Library {
        line: String,
    }

    // Slots are filled once and never freed: libraries are never unloaded.
    static HANDLERS: [AtomicPtr<Registered>; MAX_HANDLERS] =
        [const { AtomicPtr::new(null_mut()) }; MAX_HANDLERS];
    static LIBRARIES: [AtomicPtr<Library>; MAX_LIBRARIES] =
        [const { AtomicPtr::new(null_mut()) }; MAX_LIBRARIES];
    static PREVIOUS: [AtomicPtr<libc::sigaction>; SIGNALS.len()] =
        [const { AtomicPtr::new(null_mut()) }; SIGNALS.len()];
    static DUMP: AtomicBool = AtomicBool::new(true);
    static CRASHING: AtomicBool = AtomicBool::new(false);
    static INSTALL: Once = Once::new();

    pub(in crate::plugin) fn register(
        handler: MqbCrashHandler,
        user_data: *mut c_void,
    ) -> MqbStatus {
        let Some(handler) = handler else {
            return MQB_ERR_PERMANENT;
        };
        let entry = Box::into_raw(Box::new(Registered { handler, user_data }));
        if claim_slot(&HANDLERS, entry) {
            MQB_OK
        } else {
            drop(unsafe { Box::from_raw(entry) });
            MQB_ERR_PERMANENT
        }
    }

    fn claim_slot<T>(slots: &[AtomicPtr<T>], entry: *mut T) -> bool {
        slots.iter().any(|slot| {
            slot.compare_exchange(null_mut(), entry, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })
    }

    /// Remembers a loaded library for the dump; `symbol` is any address inside it.
    pub(in crate::plugin) fn record_library(path: &Path, symbol: *const c_void) {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::dladdr(symbol, &mut info) } == 0 {
            return;
        }
        let mut line = format!("{:p} {}", info.dli_fbase, path.display());
        if let Some(id) = build_id(path) {
            line.push(' ');
            line.push_str(&id);
        }
        let entry = Box::into_raw(Box::new(Library { line }));
        if !claim_slot(&LIBRARIES, entry) {
            drop(unsafe { Box::from_raw(entry) });
        }
    }

    /// The ELF build id or Mach-O UUID, in the form `file`/`dwarfdump --uuid` print.
    fn build_id(path: &Path) -> Option<String> {
        use object::{Object, ReadCache};
        let cache = ReadCache::new(std::fs::File::open(path).ok()?);
        let file = object::File::parse(&cache).ok()?;
        if let Ok(Some(id)) = file.build_id() {
            let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
            return Some(format!("build-id {hex}"));
        }
        let uuid = file.mach_uuid().ok()??;
        let hex: String = uuid.iter().map(|b| format!("{b:02X}")).collect();
        Some(format!(
            "uuid {}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
    }

    pub(in crate::plugin) fn install() {
        INSTALL.call_once(|| unsafe {
            let report = std::env::var_os("MQB_PLUGIN_CRASH_REPORT").is_none_or(|v| v != "0");
            DUMP.store(report, Ordering::Relaxed);
            warm_up_backtrace();
            for (signal, slot) in SIGNALS.iter().zip(&PREVIOUS) {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                libc::sigemptyset(&mut action.sa_mask);
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(*signal, &action, &mut previous) == 0 {
                    slot.store(Box::into_raw(Box::new(previous)), Ordering::Release);
                }
            }
        });
    }

    extern "C" fn on_signal(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        let errno = errno_location();
        let saved_errno = if errno.is_null() {
            0
        } else {
            unsafe { *errno }
        };
        let owner = claim_report();
        if owner {
            unsafe { report(signal, info, context) };
        }
        unsafe { chain(signal, info, context) };
        // Only reached when the previous handler recovered, e.g. a wasm trap.
        if owner {
            CRASHING.store(false, Ordering::Release);
        }
        if !errno.is_null() {
            unsafe { *errno = saved_errno };
        }
    }

    /// Lets one thread report; another crashing thread waits for it, then gives up.
    fn claim_report() -> bool {
        for _ in 0..MAX_WAIT_TICKS {
            if !CRASHING.swap(true, Ordering::Acquire) {
                return true;
            }
            let tick = libc::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            unsafe { libc::nanosleep(&tick, null_mut()) };
        }
        false
    }

    unsafe fn report(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        let (fault_address, code) = if info.is_null() {
            (null(), 0)
        } else {
            (unsafe { (*info).si_addr() } as *const c_void, unsafe {
                (*info).si_code
            })
        };
        let pc = unsafe { program_counter(context) };
        if DUMP.load(Ordering::Relaxed) {
            unsafe { dump(signal, fault_address, pc) };
        }
        let crash = MqbCrashInfo {
            struct_size: std::mem::size_of::<MqbCrashInfo>(),
            signal,
            code,
            fault_address,
            pc,
            siginfo: info as *const c_void,
            ucontext: context,
        };
        for slot in &HANDLERS {
            let entry = slot.load(Ordering::Acquire);
            if let Some(entry) = unsafe { entry.as_ref() } {
                unsafe { (entry.handler)(entry.user_data, &crash) };
            }
        }
    }

    /// Hands the signal to the handler that was there before ours, or dies of it.
    unsafe fn chain(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        let previous = SIGNALS
            .iter()
            .position(|s| *s == signal)
            .map(|index| PREVIOUS[index].load(Ordering::Acquire))
            .unwrap_or(null_mut());
        let mut restore: libc::sigaction = unsafe { std::mem::zeroed() };
        restore.sa_sigaction = libc::SIG_DFL;
        if let Some(previous) = unsafe { previous.as_ref() } {
            let handler = previous.sa_sigaction;
            if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                if previous.sa_flags & libc::SA_SIGINFO != 0 {
                    let handler: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                        unsafe { std::mem::transmute(handler) };
                    handler(signal, info, context);
                } else {
                    let handler: extern "C" fn(c_int) = unsafe { std::mem::transmute(handler) };
                    handler(signal);
                }
                return;
            }
            restore = *previous;
        }
        unsafe { libc::sigaction(signal, &restore, null_mut()) };
        if restore.sa_sigaction == libc::SIG_DFL {
            // Blocked until this handler returns, then fatal before anything else runs.
            unsafe { libc::raise(signal) };
        }
    }

    unsafe fn dump(signal: c_int, fault_address: *const c_void, pc: *const c_void) {
        let mut line = Line::new();
        let _ = writeln!(
            line,
            "mq-bridge: fatal signal {signal} ({}), fault address {fault_address:p}, pc {pc:p}",
            signal_name(signal)
        );
        line.flush();
        if !pc.is_null() {
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            if unsafe { libc::dladdr(pc, &mut info) } != 0 && !info.dli_fname.is_null() {
                line.push(b"mq-bridge: pc is in ");
                line.push(unsafe { CStr::from_ptr(info.dli_fname) }.to_bytes());
                let _ = write!(
                    line,
                    " at offset {:#x}",
                    pc as usize - info.dli_fbase as usize
                );
                if !info.dli_sname.is_null() {
                    line.push(b" (");
                    line.push(unsafe { CStr::from_ptr(info.dli_sname) }.to_bytes());
                    let _ = write!(line, "+{:#x})", pc as usize - info.dli_saddr as usize);
                }
                line.push(b"\n");
                line.flush();
            }
        }
        write_stderr(b"mq-bridge: plugin libraries (load address, path, build id):\n");
        for slot in &LIBRARIES {
            if let Some(library) = unsafe { slot.load(Ordering::Acquire).as_ref() } {
                write_stderr(b"mq-bridge:   ");
                write_stderr(library.line.as_bytes());
                write_stderr(b"\n");
            }
        }
        write_stderr(
            b"mq-bridge: backtrace (symbolize with `addr2line -e <library> <offset>` \
              or `atos -o <library> -l <load address> <address>`):\n",
        );
        unsafe { write_backtrace() };
    }

    fn signal_name(signal: c_int) -> &'static str {
        match signal {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGFPE => "SIGFPE",
            libc::SIGABRT => "SIGABRT",
            _ => "signal",
        }
    }

    /// A fixed buffer for formatting inside the signal handler, which must not allocate.
    struct Line {
        buf: [u8; 512],
        len: usize,
    }

    impl Line {
        fn new() -> Self {
            Self {
                buf: [0; 512],
                len: 0,
            }
        }

        fn push(&mut self, bytes: &[u8]) {
            let take = bytes.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
        }

        fn flush(&mut self) {
            write_stderr(&self.buf[..self.len]);
            self.len = 0;
        }
    }

    impl std::fmt::Write for Line {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.push(text.as_bytes());
            Ok(())
        }
    }

    fn write_stderr(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
            if written <= 0 {
                return;
            }
            bytes = &bytes[written as usize..];
        }
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    extern "C" {
        fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
        fn backtrace_symbols_fd(buffer: *const *mut c_void, size: c_int, fd: c_int);
    }
    #[cfg(target_vendor = "apple")]
    use libc::{backtrace, backtrace_symbols_fd};

    #[cfg(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu")))]
    unsafe fn write_backtrace() {
        let mut frames = [null_mut(); MAX_FRAMES];
        let len = unsafe { backtrace(frames.as_mut_ptr(), MAX_FRAMES as c_int) };
        unsafe { backtrace_symbols_fd(frames.as_ptr(), len, 2) };
    }

    #[cfg(not(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu"))))]
    unsafe fn write_backtrace() {
        write_stderr(b"mq-bridge:   unavailable on this platform\n");
    }

    /// glibc loads libgcc_s on the first `backtrace`, which allocates.
    fn warm_up_backtrace() {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            let mut frames = [null_mut(); 1];
            backtrace(frames.as_mut_ptr(), 1);
        }
    }

    fn errno_location() -> *mut c_int {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return unsafe { libc::__errno_location() };
        #[cfg(target_vendor = "apple")]
        return unsafe { libc::__error() };
        #[allow(unreachable_code)]
        null_mut()
    }

    /// The interrupted instruction from the handler's `ucontext_t`.
    unsafe fn program_counter(context: *mut c_void) -> *const c_void {
        if context.is_null() {
            return null();
        }
        #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
        return unsafe {
            (*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize]
        } as *const c_void;
        #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "aarch64"))]
        return unsafe { (*(context as *const libc::ucontext_t)).uc_mcontext.pc } as *const c_void;
        // libc has no Darwin ucontext_t; offsets are from <sys/_types/_ucontext.h>
        // and <mach/*/_structs.h>: uc_mcontext at 48, then __es (16) and __ss.
        #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
        return unsafe {
            let mcontext = *(context.cast::<u8>().add(48) as *const *const u8);
            *(mcontext.add(16 + 256) as *const *const c_void)
        };
        #[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
        return unsafe {
            let mcontext = *(context.cast::<u8>().add(48) as *const *const u8);
            *(mcontext.add(16 + 128) as *const *const c_void)
        };
        #[allow(unreachable_code)]
        null()
    }
}

#[cfg(not(unix))]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;

    use crate::support::plugin_abi::{MqbCrashHandler, MqbStatus, MQB_ERR_UNSUPPORTED};

    pub(in crate::plugin) fn install() {}

    pub(in crate::plugin) fn record_library(_: &Path, _: *const c_void) {}

    pub(in crate::plugin) fn register(_: MqbCrashHandler, _: *mut c_void) -> MqbStatus {
        MQB_ERR_UNSUPPORTED
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::c_void;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use crate::support::plugin_abi::{MqbCrashInfo, MQB_ERR_PERMANENT, MQB_OK};

    const CHILD: &str = "MQB_CRASH_TEST_CHILD";

    unsafe extern "C" fn on_crash(user_data: *mut c_void, info: *const MqbCrashInfo) {
        let marker: &[u8] = if unsafe { (*info).pc }.is_null() {
            b"handler ran without pc\n"
        } else {
            b"handler ran with pc\n"
        };
        assert_eq!(user_data as usize, 7);
        unsafe { libc::write(2, marker.as_ptr().cast(), marker.len()) };
    }

    /// Crashes on purpose; only does anything when run by [`crash`].
    #[test]
    fn crash_child() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        super::install();
        super::record_library(&exe, crash_child as *const c_void);
        unsafe {
            assert_eq!(
                super::register_crash_handler(None, std::ptr::null_mut()),
                MQB_ERR_PERMANENT
            );
            assert_eq!(
                super::register_crash_handler(Some(on_crash), 7 as *mut c_void),
                MQB_OK
            );
            std::ptr::read_volatile(std::hint::black_box(8 as *const u8));
        }
    }

    fn crash(report: Option<&str>) -> (Option<i32>, String) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "plugin::crash::tests::crash_child",
                "--nocapture",
            ])
            .env(CHILD, "1");
        if let Some(report) = report {
            command.env("MQB_PLUGIN_CRASH_REPORT", report);
        }
        let output = command.output().unwrap();
        (
            output.status.signal(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    fn a_crash_is_reported_then_kills_the_process_with_its_signal() {
        let (signal, stderr) = crash(None);
        assert!(
            matches!(signal, Some(libc::SIGSEGV | libc::SIGBUS)),
            "{signal:?}\n{stderr}"
        );
        assert!(stderr.contains("mq-bridge: fatal signal"), "{stderr}");
        assert!(stderr.contains("fault address 0x8"), "{stderr}");
        assert!(stderr.contains("mq-bridge: pc is in "), "{stderr}");
        assert!(stderr.contains("mq-bridge: backtrace"), "{stderr}");
        let exe = std::env::current_exe().unwrap();
        assert!(stderr.contains(&exe.display().to_string()), "{stderr}");
        assert!(stderr.contains("handler ran with pc"), "{stderr}");
    }

    #[test]
    fn the_dump_can_be_turned_off_but_handlers_still_run() {
        let (signal, stderr) = crash(Some("0"));
        assert!(
            matches!(signal, Some(libc::SIGSEGV | libc::SIGBUS)),
            "{signal:?}\n{stderr}"
        );
        assert!(!stderr.contains("mq-bridge: fatal signal"), "{stderr}");
        assert!(stderr.contains("handler ran with pc"), "{stderr}");
    }
}
