#![allow(dead_code, unused_variables, async_fn_in_trait)]
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::io::Io;
use crate::segment::sealed::SealedSegment;
use libsql_sys::name::NamespaceName;

use self::job::JobResult;
use self::scheduler::Scheduler;
use self::storage::Storage;

mod job;
mod restore;
mod scheduler;
pub mod storage;

/// Backgroung loop task state.
///
/// The background loop task is not allowed to exit, unless it was notified for shutdown.
///
/// On shutdown, attempts to empty the queue, and flush the receiver. When the last handle of the
/// receiver is dropped, and the queue is empty, exit.
pub struct BottomlessLoop<S: Storage, FS: Io> {
    receiver: mpsc::Receiver<StoreSegmentRequest<S::Config, Arc<SealedSegment<FS::File>>>>,
    scheduler: Scheduler<S::Config, Arc<SealedSegment<FS::File>>>,
    storage: Arc<S>,
    filesystem: Arc<FS>,
    max_in_flight: usize,
    in_flight_futs: JoinSet<JobResult<S::Config, Arc<SealedSegment<FS::File>>>>,
    force_shutdown: oneshot::Receiver<()>,
}

impl<S, FS> BottomlessLoop<S, FS>
where
    FS: Io,
    S: Storage + 'static,
{
    /// Schedules durability jobs. This loop is not allowed to fail, or lose jobs.
    /// A job is prepared by calling `Scheduler::prepare(..)`. The job is spawned, and it returns a
    /// `JobResult`, which is then returned to the scheduler by calling `Scheduler::report(..)`.
    /// When a request is received, it is immediately scheduled by calling `Scheduler::register`
    /// with it.
    ///
    /// The loop is only allowed to shutdown if the receiver is closed, and the scheduler is empty,
    /// or if `force_shutdown` is called, in which case everything is dropped in place.
    #[tracing::instrument(skip(self))]
    async fn run(mut self) {
        let mut shutting_down = false;
        // run the loop until shutdown.
        loop {
            if shutting_down && self.scheduler.is_empty() {
                break;
            }

            // schedule as much work as possible
            while self.scheduler.has_work() && self.in_flight_futs.len() < self.max_in_flight {
                let job = self
                    .scheduler
                    .schedule()
                    .expect("scheduler has work, but didn't return a job");
                self.in_flight_futs
                    .spawn(job.perform(self.storage.clone(), self.filesystem.clone()));
            }

            tokio::select! {
                biased;
                Some(join_result) = self.in_flight_futs.join_next(), if !self.in_flight_futs.is_empty() => {
                    match join_result {
                        Ok(job_result) => {
                            // if shutting down, log progess:
                            if shutting_down {
                                tracing::info!("processed job, {} jobs remaining", self.in_flight_futs.len());
                            }
                            self.scheduler.report(job_result).await;
                        }
                        Err(e) => {
                            // job panicked. report and exit process. The program is crippled, from
                            // now on, so we just exit, and hope to restart on a fresh state.
                            tracing::error!("fatal error: bottomless job panicked: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                msg = self.receiver.recv(), if !shutting_down => {
                    match msg {
                        Some(req) => {
                            self.scheduler.register(req);
                        }
                        None => {
                            shutting_down = true;
                        }
                    }
                }
                shutdown = &mut self.force_shutdown => {
                    if shutdown.is_ok() {
                        break
                    } else {
                        // force_shutdown sender was dropped without sending a message (likely a
                        // bug). Log and default to graceful shutdown.
                        // tracing::error!("bottomless force shutdown handle dropped without notifying; shutting down gracefully");
                    }
                }
            }
        }
    }
}

pub struct BottomlessConfig<C> {
    /// The maximum number of store jobs that can be processed conccurently
    max_jobs_conccurency: usize,
    /// The maximum number of jobs that can be enqueued before throttling
    max_enqueued_jobs: usize,
    config: C,
}

pub struct Bottomless<C, S> {
    /// send request to the main loop
    job_sender: mpsc::Sender<StoreSegmentRequest<C, S>>,
    /// receiver for the current max durable index
    durable_notifier: mpsc::Receiver<(NamespaceName, u64)>,
    /// join handle to the `BottomlessLoop`
    loop_handle: JoinHandle<()>,
    force_shutdown: oneshot::Sender<()>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("an error occured while storing a segment: {0}")]
    Store(String),
    #[error("unable to find the requested frame_no: {0}")]
    FrameNotFound(u64),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl<C, F> Bottomless<C, F> {
    pub async fn new<S: Storage>(_storage: S) -> Result<Bottomless<S::Config, F>> {
        todo!()
    }
    /// Send a request make a segment durable. Return a future that resolves when that segment
    /// becomes durable.
    pub async fn store(&self, _request: StoreSegmentRequest<C, F>) {
        assert!(
            !self.job_sender.is_closed(),
            "bottomless loop was closed before the handle was dropped"
        );
        todo!();
    }

    /// Tries to shutdown bottomless gracefully.
    /// If timeout expires, bottomless is forcefully shutdown.
    pub async fn shutdown(self, timeout: Duration) {
        let (mut handle, force_shutdown) = {
            // we drop the sender, the loop will finish processing scheduled job and exit
            // gracefully.
            let Self {
                loop_handle,
                force_shutdown,
                ..
            } = self;
            (loop_handle, force_shutdown)
        };

        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(_) => (),
            Err(_) => {
                tracing::error!("Bottomless graceful shutdown elapsed, shutting down forcefully");
                let _ = force_shutdown.send(());
                handle
                    .await
                    .expect("bottomless loop panicked while shutting down");
            }
        }
    }
}

#[derive(Debug)]
pub struct StoreSegmentRequest<C, T> {
    namespace: NamespaceName,
    /// Path to the segment. Read-only for bottomless
    segment: T,
    /// When this segment was created
    created_at: DateTime<Utc>,
    /// alternative configuration to use with the storage layer.
    /// e.g: S3 overrides
    storage_config_override: Option<Arc<C>>,
}
