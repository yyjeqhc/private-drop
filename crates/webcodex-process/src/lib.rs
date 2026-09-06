//! Managed process trees for WebCodex.
//!
//! This crate provides a cross-platform handle to a child process together
//! with the entire descendant tree that it spawns. The owned entity is the
//! *tree*, not just the direct child:
//!
//! * On Unix the child is placed in a private process group; tree-wide
//!   termination signals the whole group.
//! * On Windows the child is assigned to a private Job Object configured with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so that dropping the last job handle
//!   forcibly terminates every process in the tree.
//!
//! The key lifecycle distinctions, which every platform backend honours:
//!
//! 1. [`ManagedChild::wait`] / [`ManagedChild::try_wait`] wait **only** for the
//!    direct child. A tree that owns a running grandchild is *not* considered
//!    exited just because the direct child finished.
//! 2. [`ManagedChild::terminate_tree`] forcibly terminates the entire owned
//!    process tree.
//! 3. [`ManagedChild::wait_tree_exit`] waits until the tree contains no live
//!    processes.
//! 4. [`Drop`] is a fail-safe backstop: it forcibly terminates the tree, but
//!    explicit cleanup must be preferred. It never panics, never blocks for a
//!    long time, and never spawns external commands.
//!
//! This crate deliberately does **not** expose a way to detach the child or
//! hand back the tree: a [`ManagedChild`] owns the tree for its whole life.

#[cfg(not(any(unix, windows)))]
compile_error!("webcodex-process supports only unix and windows targets");

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::ManagedChild;

#[cfg(windows)]
pub use windows::ManagedChild;

/// Outcome of a graceful tree-termination request
/// ([`ManagedChild::request_terminate_tree`]).
///
/// The three states are deliberately explicit rather than a boolean: callers
/// must be able to distinguish "the graceful signal was delivered" from "the
/// tree had already exited" from "this platform cannot do graceful tree
/// termination at all", because each requires a different follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GracefulTermination {
    /// The graceful termination signal was delivered to the owned process tree.
    Requested,
    /// The owned tree had already fully exited before the request.
    AlreadyExited,
    /// Graceful tree termination is not supported on this platform.
    Unsupported,
}

/// Options that influence how a [`ManagedChild`] is spawned.
///
/// All fields have platform-neutral defaults; set only what you need.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnOptions {
    /// Extra Windows process creation flags.
    ///
    /// [`ManagedChild::spawn`] owns the Windows creation flags because the
    /// standard library has no stable API for reading flags already stored in
    /// a [`std::process::Command`]. Flags previously set directly on the
    /// command are therefore replaced. Supply all required extra flags here;
    /// `CREATE_SUSPENDED` is added internally only for the managed spawn and is
    /// removed from the reusable command before the method returns.
    ///
    /// Ignored on Unix. Defaults to `0`.
    pub windows_creation_flags: u32,

    /// Let children created by this managed process leave its Windows Job
    /// Object without requiring `CREATE_BREAKAWAY_FROM_JOB`.
    ///
    /// This is intentionally narrow: use it only when the direct child is a
    /// trusted process supervisor that immediately establishes its own process
    /// ownership for descendants. It prevents incompatible nested Job Object
    /// membership while keeping the direct child itself owned by this
    /// [`ManagedChild`]. Ignored on Unix. Defaults to `false`.
    pub windows_silent_child_breakaway: bool,
}

impl SpawnOptions {
    /// Default options: no extra creation flags.
    pub fn new() -> Self {
        Self::default()
    }
}
