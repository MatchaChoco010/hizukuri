//! Building a render pipeline away from the thread that owns the device.
//!
//! Creating a pipeline makes the driver compile its shaders, which takes
//! tens of milliseconds. [`crate::Gpu::prepare_render_pipeline`] checks the
//! description at once and returns a [`PipelineJob`] that does the slow part
//! on whatever thread runs it, without the `Gpu`; the `Gpu` asks for the
//! result with [`crate::Gpu::finish_render_pipeline`], which answers "not
//! yet" until it is there.
//!
//! What makes this safe:
//!
//! - The job owns copies of everything it reads (the description, the
//!   shaders' code and interfaces), so destroying the shaders meanwhile does
//!   not reach it.
//! - Creating a pipeline needs no external synchronization on the device,
//!   and touches neither the command buffers nor the descriptor heaps.
//! - The device outlives the job: dropping the `Gpu` first waits for the
//!   jobs that are running, and a job that starts after that does nothing.
//! - A pipeline nobody asks for any more (cancelled, or finished after the
//!   `Gpu` was dropped) is destroyed, not leaked.

use std::collections::{HashMap, HashSet};
use std::mem::DropGuard;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use vulkan::vk;

use crate::desc::{PipelineExtra, RenderPipelineDesc};
use crate::error::{ErrorCode, GfxError, Result};
use crate::gpu::{HeapStrides, PipelineBinding, StageCode, build_render_pipeline};

/// What the `Gpu` and its jobs share.
pub(crate) struct Background {
    state: Mutex<State>,
    /// Signalled when a job stops running.
    idle: Condvar,
}

#[derive(Default)]
struct State {
    /// The `Gpu` is being dropped: jobs must not touch the device any more.
    closed: bool,
    /// Jobs that are inside the driver right now.
    running: usize,
    /// Results nobody has taken yet, by ticket.
    done: HashMap<u64, Result<vk::Pipeline>>,
    /// Tickets whose result is to be thrown away when it arrives.
    cancelled: HashSet<u64>,
}

impl Background {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            idle: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent across a panic: every change is a single
        // insertion, removal or counter step.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The result of a job, once, when it is there.
    pub fn take(&self, ticket: u64) -> Option<Result<vk::Pipeline>> {
        self.lock().done.remove(&ticket)
    }

    /// Gives up on a job. Returns its pipeline if it was built already (the
    /// caller destroys it); otherwise the job destroys it when it is done.
    pub fn cancel(&self, ticket: u64) -> Option<vk::Pipeline> {
        let mut state = self.lock();
        match state.done.remove(&ticket) {
            Some(result) => result.ok(),
            None => {
                state.cancelled.insert(ticket);
                None
            }
        }
    }

    /// Stops jobs from starting, waits for the running ones (unless told not
    /// to), and returns the pipelines nobody took.
    pub fn close(&self, wait: bool) -> Vec<vk::Pipeline> {
        let mut state = self.lock();
        state.closed = true;
        while wait && state.running > 0 {
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.done.drain().filter_map(|(_, r)| r.ok()).collect()
    }
}

/// A render pipeline that is being built. [`crate::Gpu::finish_render_pipeline`]
/// turns it into a handle once it is ready.
pub struct PendingPipeline {
    pub(crate) ticket: u64,
    pub(crate) owner: Arc<Background>,
}

impl std::fmt::Debug for PendingPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PendingPipeline({})", self.ticket)
    }
}

/// The slow part of creating a render pipeline. Send it to another thread
/// and call [`PipelineJob::run`] there. Dropping it without running it makes
/// the pending pipeline finish with an error.
pub struct PipelineJob {
    inner: Option<Inner>,
}

pub(crate) struct Inner {
    pub ticket: u64,
    pub shared: Arc<Background>,
    pub device: vulkan::Device,
    pub strides: HeapStrides,
    pub desc: RenderPipelineDesc,
    pub extra: PipelineExtra,
    pub vertex: StageCode,
    pub fragment: StageCode,
    pub bindings: Vec<PipelineBinding>,
}

impl PipelineJob {
    pub(crate) fn new(inner: Inner) -> Self {
        Self { inner: Some(inner) }
    }

    /// Builds the pipeline. Blocks for as long as the driver takes.
    pub fn run(mut self) {
        let Some(job) = self.inner.take() else {
            return;
        };
        {
            let mut state = job.shared.lock();
            if state.closed {
                return;
            }
            state.running += 1;
        }
        // Counts the job as running until it is dropped, also if the job
        // panics.
        let running = DropGuard::new(&*job.shared, |shared: &Background| {
            shared.lock().running -= 1;
            shared.idle.notify_all();
        });
        // SAFETY: the description, the shaders and the bindings passed the
        // checks of `Gpu::prepare_render_pipeline`. The device is alive:
        // `Background::close`, which precedes its destruction, waits while
        // this job counts as running.
        let result = unsafe {
            build_render_pipeline(
                &job.device,
                job.strides,
                &job.desc,
                &job.extra,
                job.vertex.as_ref(),
                job.fragment.as_ref(),
                &job.bindings,
            )
        };
        let mut state = job.shared.lock();
        if state.closed || state.cancelled.remove(&job.ticket) {
            if let Ok(pipeline) = result {
                // SAFETY: the pipeline was just created and nothing uses it;
                // the device is alive, as above.
                unsafe { job.device.destroy_pipeline(pipeline, None) };
            }
        } else {
            state.done.insert(job.ticket, result);
        }
        drop(state);
        drop(running);
    }
}

impl Drop for PipelineJob {
    fn drop(&mut self) {
        let Some(job) = self.inner.take() else {
            return;
        };
        let mut state = job.shared.lock();
        if !state.closed && !state.cancelled.remove(&job.ticket) {
            state.done.insert(
                job.ticket,
                Err(GfxError::new(
                    ErrorCode::InvalidState,
                    "the job that builds the pipeline was dropped before it ran",
                )),
            );
        }
    }
}
