/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! APIs for memory profiling.

#![deny(missing_docs)]

use std::cell::{LazyCell, RefCell};
use std::collections::HashSet;
use std::ffi::c_void;
use std::marker::Send;

use crossbeam_channel::Sender;
use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use log::warn;
use malloc_size_of::MallocSizeOfOps;
use serde::{Deserialize, Serialize};

/// A trait to abstract away the various kinds of message senders we use.
pub trait OpaqueSender<T> {
    /// Send a message.
    fn send(&self, message: T);
}

impl<T> OpaqueSender<T> for Sender<T> {
    fn send(&self, message: T) {
        if let Err(e) = Sender::send(self, message) {
            warn!(
                "Error communicating with the target thread from the profiler: {:?}",
                e
            );
        }
    }
}

impl<T> OpaqueSender<T> for IpcSender<T>
where
    T: serde::Serialize,
{
    fn send(&self, message: T) {
        if let Err(e) = IpcSender::send(self, message) {
            warn!(
                "Error communicating with the target thread from the profiler: {}",
                e
            );
        }
    }
}

/// Front-end representation of the profiler used to communicate with the
/// profiler.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfilerChan(pub IpcSender<ProfilerMsg>);

/// A handle that encompasses a registration with the memory profiler.
/// The registration is tied to the lifetime of this type; the memory
/// profiler unregister the reporter when this object is dropped.
pub struct ProfilerRegistration {
    sender: ProfilerChan,
    reporter_name: String,
}

impl Drop for ProfilerRegistration {
    fn drop(&mut self) {
        self.sender
            .send(ProfilerMsg::UnregisterReporter(self.reporter_name.clone()));
    }
}

impl ProfilerChan {
    /// Send `msg` on this `IpcSender`.
    ///
    /// Warns if the send fails.
    pub fn send(&self, msg: ProfilerMsg) {
        if let Err(e) = self.0.send(msg) {
            warn!("Error communicating with the memory profiler thread: {}", e);
        }
    }

    /// Register a new reporter and return a handle to automatically
    /// unregister it in the future.
    pub fn prepare_memory_reporting<M, T, C>(
        &self,
        reporter_name: String,
        channel_for_reporter: C,
        msg: M,
    ) -> ProfilerRegistration
    where
        M: Fn(ReportsChan) -> T + Send + 'static,
        T: Send + 'static,
        C: OpaqueSender<T> + Send + 'static,
    {
        // Register the memory reporter.
        let (reporter_sender, reporter_receiver) = ipc::channel().unwrap();
        ROUTER.add_typed_route(
            reporter_receiver,
            Box::new(move |message| {
                // Just injects an appropriate event into the paint thread's queue.
                let request: ReporterRequest = message.unwrap();
                channel_for_reporter.send(msg(request.reports_channel));
            }),
        );
        self.send(ProfilerMsg::RegisterReporter(
            reporter_name.clone(),
            Reporter(reporter_sender),
        ));

        ProfilerRegistration {
            sender: self.clone(),
            reporter_name,
        }
    }

    /// Runs `f()` with memory profiling.
    pub fn run_with_memory_reporting<F, M, T, C>(
        &self,
        f: F,
        reporter_name: String,
        channel_for_reporter: C,
        msg: M,
    ) where
        F: FnOnce(),
        M: Fn(ReportsChan) -> T + Send + 'static,
        T: Send + 'static,
        C: OpaqueSender<T> + Send + 'static,
    {
        let _registration = self.prepare_memory_reporting(reporter_name, channel_for_reporter, msg);

        f();
    }
}

/// The various kinds of memory measurement.
///
/// Here "explicit" means explicit memory allocations done by the application. It includes
/// allocations made at the OS level (via functions such as VirtualAlloc, vm_allocate, and mmap),
/// allocations made at the heap allocation level (via functions such as malloc, calloc, realloc,
/// memalign, operator new, and operator new[]) and where possible, the overhead of the heap
/// allocator itself. It excludes memory that is mapped implicitly such as code and data segments,
/// and thread stacks. "explicit" is not guaranteed to cover every explicit allocation, but it does
/// cover most (including the entire heap), and therefore it is the single best number to focus on
/// when trying to reduce memory usage.
#[derive(Debug, Deserialize, Serialize)]
pub enum ReportKind {
    /// A size measurement for an explicit allocation on the jemalloc heap. This should be used
    /// for any measurements done via the `MallocSizeOf` trait.
    ExplicitJemallocHeapSize,

    /// A size measurement for an explicit allocation on the system heap. Only likely to be used
    /// for external C or C++ libraries that don't use jemalloc.
    ExplicitSystemHeapSize,

    /// A size measurement for an explicit allocation not on the heap, e.g. via mmap().
    ExplicitNonHeapSize,

    /// A size measurement for an explicit allocation whose location is unknown or uncertain.
    ExplicitUnknownLocationSize,

    /// A size measurement for a non-explicit allocation. This kind is used for global
    /// measurements such as "resident" and "vsize", and also for measurements that cross-cut the
    /// measurements grouped under "explicit", e.g. by grouping those measurements in a way that's
    /// different to how they are grouped under "explicit".
    NonExplicitSize,
}

/// A single memory-related measurement.
#[derive(Debug, Deserialize, Serialize)]
pub struct Report {
    /// The identifying path for this report.
    pub path: Vec<String>,

    /// The report kind.
    pub kind: ReportKind,

    /// The size, in bytes.
    pub size: usize,
}

/// A set of reports belonging to a process.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProcessReports {
    /// The set of reports.
    pub reports: Vec<Report>,

    /// The process id.
    pub pid: u32,
}

impl ProcessReports {
    /// Adopt these reports and configure the process pid.
    pub fn new(reports: Vec<Report>) -> Self {
        Self {
            reports,
            pid: std::process::id(),
        }
    }
}

/// A channel through which memory reports can be sent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReportsChan(pub IpcSender<ProcessReports>);

impl ReportsChan {
    /// Send `report` on this `IpcSender`.
    ///
    /// Panics if the send fails.
    pub fn send(&self, reports: ProcessReports) {
        self.0.send(reports).unwrap();
    }
}

/// The protocol used to send reporter requests.
#[derive(Debug, Deserialize, Serialize)]
pub struct ReporterRequest {
    /// The channel on which reports are to be sent.
    pub reports_channel: ReportsChan,
}

/// A memory reporter is capable of measuring some data structure of interest. It's structured as
/// an IPC sender that a `ReporterRequest` in transmitted over. `ReporterRequest` objects in turn
/// encapsulate the channel on which the memory profiling information is to be sent.
///
/// In many cases, clients construct `Reporter` objects by creating an IPC sender/receiver pair and
/// registering the receiving end with the router so that messages from the memory profiler end up
/// injected into the client's event loop.
#[derive(Debug, Deserialize, Serialize)]
pub struct Reporter(pub IpcSender<ReporterRequest>);

impl Reporter {
    /// Collect one or more memory reports. Returns true on success, and false on failure.
    pub fn collect_reports(&self, reports_channel: ReportsChan) {
        self.0.send(ReporterRequest { reports_channel }).unwrap()
    }
}

/// An easy way to build a path for a report.
#[macro_export]
macro_rules! path {
    ($($x:expr),*) => {{
        use std::borrow::ToOwned;
        vec![$( $x.to_owned() ),*]
    }}
}

/// The results produced by the memory reporter.
#[derive(Debug, Deserialize, Serialize)]
pub struct MemoryReportResult {
    /// All the results from the MemoryReports
    pub results: Vec<MemoryReport>,
}

#[derive(Debug, Deserialize, Serialize)]
/// A simple memory report
pub struct MemoryReport {
    /// The pid of the report
    pub pid: u32,
    /// Is this the main process
    pub is_main_process: bool,
    /// All the reports for this pid
    pub reports: Vec<Report>,
}

/// Messages that can be sent to the memory profiler thread.
#[derive(Debug, Deserialize, Serialize)]
pub enum ProfilerMsg {
    /// Register a Reporter with the memory profiler. The String is only used to identify the
    /// reporter so it can be unregistered later. The String must be distinct from that used by any
    /// other registered reporter otherwise a panic will occur.
    RegisterReporter(String, Reporter),

    /// Unregister a Reporter with the memory profiler. The String must match the name given when
    /// the reporter was registered. If the String does not match the name of a registered reporter
    /// a panic will occur.
    UnregisterReporter(String),

    /// Tells the memory profiler to shut down.
    Exit,

    /// Triggers sending back the memory profiling metrics,
    Report(IpcSender<MemoryReportResult>),
}

thread_local!(static SEEN_POINTERS: LazyCell<RefCell<HashSet<*const c_void>>> = const {
    LazyCell::new(Default::default)
});

/// Invoke the provided function after initializing the memory profile tools.
/// The function is expected to call all the desired [MallocSizeOf::size_of]
/// for allocations reachable from the current thread.
pub fn perform_memory_report<F: FnOnce(&mut MallocSizeOfOps)>(f: F) {
    let seen_pointer = move |ptr| SEEN_POINTERS.with(|pointers| !pointers.borrow_mut().insert(ptr));
    let mut ops = MallocSizeOfOps::new(
        servo_allocator::usable_size,
        None,
        Some(Box::new(seen_pointer)),
    );
    f(&mut ops);
    SEEN_POINTERS.with(|pointers| {
        let mut pointers = pointers.borrow_mut();
        pointers.clear();
        pointers.shrink_to_fit();
    });
}
