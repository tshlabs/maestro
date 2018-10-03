//
//
//

//!

#[macro_use]
extern crate clap;
extern crate crossbeam_channel;
extern crate libc;
extern crate nix;
extern crate signal_hook;

use clap::{App, Arg, ArgMatches};
use crossbeam_channel::Receiver;
use libc::pid_t;
use nix::sys::signal::{kill, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;
use std::cell::Cell;
use std::process::{self, Command};
use std::sync::{Arc, Mutex};
use std::{env, fmt, thread};

const CHANNEL_CAP: usize = 32;

/// All signals that can and should be caught an forwarded to our child process.
///
/// These signals will be caught and forwarded on a single thread in this process
/// and masked in all other threads.
const SIGNALS_TO_HANDLE: &[Signal] = &[
    Signal::SIGABRT,
    Signal::SIGALRM,
    Signal::SIGBUS,
    Signal::SIGCHLD,
    Signal::SIGCONT,
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGIO,
    Signal::SIGPIPE,
    Signal::SIGPROF,
    Signal::SIGQUIT,
    Signal::SIGSYS,
    Signal::SIGTERM,
    Signal::SIGTRAP,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGWINCH,
];

/// Selectively mask or unmask a set of signals for the current thread.
///
/// The signals supplied will be blocked or unblocked depending on the method
/// called. This does not modify any existing masks. However, the signals blocked
/// and unblocked by default are nearly all signals that a process could actually
/// catch or would want to catch.
struct ThreadMasker {
    mask: SigSet,
}

impl ThreadMasker {
    /// Set the allowed signals that will be blocked or unblocked.
    fn new(allowed: &[Signal]) -> Self {
        // Start from an empty set of signals and only add the ones that we expect
        // to handle and hence need to mask from all threads that *aren't* specifically
        // for handling signals.
        let mut mask = SigSet::empty();

        for sig in allowed {
            mask.add(*sig);
        }

        ThreadMasker { mask }
    }

    /// Explicitly allow the registered signals for the thread this method is run in.
    ///
    /// # Panics
    ///
    /// This method will panic if the thread signal mask cannot be set.
    fn allow_for_thread(&self) {
        self.mask.thread_unblock().unwrap();
    }

    /// Explicitly block the registered signals for the thread this method is run in.
    ///
    /// # Panics
    ///
    /// This method will panic if the thread signal mask cannot be set.
    fn block_for_thread(&self) {
        self.mask.thread_block().unwrap();
    }
}

impl fmt::Debug for ThreadMasker {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut signals = vec![];

        for sig in Signal::iterator() {
            if self.mask.contains(sig) {
                signals.push(sig as i32);
            }
        }

        write!(f, "ThreadMasker {{ mask: {:?} }}", signals)
    }
}

/// Holder for the PID of the child process we launch.
///
/// This exists because the thread that forwards signals to the child needs its
/// PID but the child hasn't been launched yet at the time the thread to forward
/// signals is created.
#[derive(Debug)]
struct ChildPid {
    pid: Mutex<Cell<Option<Pid>>>,
}

impl ChildPid {
    /// Get the PID of the child if it has been set, `None` if it hasn't yet
    fn get_pid(&self) -> Option<Pid> {
        let cell = self.pid.lock().unwrap();
        cell.get()
    }

    /// Set the PID of the child process.
    fn set_pid(&self, pid: Pid) {
        let cell = self.pid.lock().unwrap();
        cell.set(Some(pid))
    }
}

impl From<pid_t> for ChildPid {
    fn from(pid: pid_t) -> Self {
        Self::from(Pid::from_raw(pid))
    }
}

impl From<Pid> for ChildPid {
    fn from(pid: Pid) -> Self {
        ChildPid {
            pid: Mutex::new(Cell::new(Some(pid))),
        }
    }
}

impl Default for ChildPid {
    fn default() -> Self {
        ChildPid {
            pid: Mutex::new(Cell::new(None)),
        }
    }
}

/// Launch a thread specifically for receiving signals and forwarding them to
/// another thread via a crossbeam channel.
///
/// This will take care of unmasking the desired signals for the thread launched.
#[derive(Debug)]
struct SignalCatcher {
    signals: Signals,
    masker: ThreadMasker,
}

impl SignalCatcher {
    fn new(allowed: &[Signal]) -> Self {
        let allowed_ints: Vec<i32> = allowed.iter().map(|s| *s as i32).collect();

        SignalCatcher {
            signals: Signals::new(allowed_ints).unwrap(),
            masker: ThreadMasker::new(allowed),
        }
    }

    /// Spawn a thread that will receive signals forever and forward them via
    /// the returned crossbeam channel `Receiver` instance.
    ///
    /// The channel used has a finite capacity.
    fn launch(self) -> Receiver<Signal> {
        let (send, recv) = crossbeam_channel::bounded(CHANNEL_CAP);
        thread::spawn(move || {
            self.masker.allow_for_thread();

            for sig in self.signals.forever() {
                send.send(Signal::from_c_int(sig).unwrap());
            }
        });

        recv
    }
}

/// Forward received signals to the child process we launched and clean up after
/// any child processes or ours (or of our child) that exit.
///
/// This will take care of blocking the signals that should be handled by a different
/// thread.
#[derive(Debug)]
struct SignalHandler {
    receiver: Receiver<Signal>,
    child: Arc<ChildPid>,
    masker: ThreadMasker,
}

impl SignalHandler {
    /// Set the channel for receiving signals, PID of our child, and list of signals
    /// that should be blocked in our thread since they are being handled elsewhere.
    fn new(receiver: Receiver<Signal>, child: Arc<ChildPid>, allowed: &[Signal]) -> Self {
        SignalHandler {
            receiver,
            child,
            masker: ThreadMasker::new(allowed),
        }
    }

    /// Take all appropriate action for the signal including forwarding it to the child PID.
    fn dispatch(pid: Pid, sig: Signal) {
        if sig == Signal::SIGCHLD {
            Self::wait_child();
        }

        Self::propagate(pid, sig);
    }

    /// Use `waitpid` to cleanup after any children that have changed state.
    fn wait_child() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => {
                    break;
                }
                _ => (),
            };
        }
    }

    /// Send the given signal to our child process.
    fn propagate(pid: Pid, sig: Signal) {
        // It's possible that the process has already died by the time we attempt to
        // send this signal so we don't really care if it's successful here, just try
        // to send it and ignore any failures.
        let _ = kill(pid, sig);
    }

    /// Spawn a thread that will receive signals from another thread via a crossbeam
    /// channel and propagate them to the child process launched as well as clean up
    /// after any children (via `libc::waitpid`).
    fn launch(self) {
        thread::spawn(move || {
            self.masker.block_for_thread();

            for sig in self.receiver {
                let pid = self.child.get_pid();

                if let Some(p) = pid {
                    Self::dispatch(p, sig);
                }
            }
        });
    }
}

fn parse_cli_opts<'a>(args: Vec<String>) -> ArgMatches<'a> {
    App::new("PID 1")
        .version(crate_version!())
        .set_term_width(72)
        .about("\nIt does PID 1 things")
        .arg(Arg::with_name("arguments").multiple(true).help(
            "Command to execute and arguments to it. Note that the command \
             must be an absolute path. For example `/usr/bin/whatever`, not just \
             `whatever`. Any arguments to pass to the command should be listed as \
             well, separated with spaces.",
        ))
        .get_matches_from(args)
}

fn main() {
    let matches = parse_cli_opts(env::args().collect());
    let arguments = values_t!(matches, "arguments", String).unwrap_or_else(|e| e.exit());

    let masker = ThreadMasker::new(SIGNALS_TO_HANDLE);
    masker.block_for_thread();

    let catcher = SignalCatcher::new(SIGNALS_TO_HANDLE);
    let receiver = catcher.launch();

    let pid = Arc::new(ChildPid::default());
    let pid_clone = Arc::clone(&pid);

    let handler = SignalHandler::new(receiver, pid_clone, SIGNALS_TO_HANDLE);
    handler.launch();

    let mut child = match Command::new(&arguments[0]).args(&arguments[1..]).spawn() {
        Err(e) => {
            eprintln!("blag: command error: {}", e);
            process::exit(1);
        }
        Ok(c) => c,
    };

    pid.set_pid(Pid::from_raw(child.id() as pid_t));
    let status = match child.wait() {
        Err(e) => {
            eprintln!("blag: wait error: {}", e);
            process::exit(1);
        }
        Ok(s) => s,
    };

    if let Some(code) = status.code() {
        process::exit(code);
    } else {
        process::exit(0);
    }
}
