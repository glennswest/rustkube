//! scheduler: pod and VirtualMachineInstance placement.
//!
//! Once a second, lists pods with an empty `spec.nodeName` (and unplaced
//! VirtualMachineInstances), runs the fixed filter and score functions in
//! [`filter`] and [`score`], and binds each to the best node. It does not
//! preempt ([`preemption`] is not called, #84) and ignores `schedulingGates`
//! (#87).

pub mod affinity;
pub mod filter;
pub mod leaderelection;
pub mod metrics_server;
pub mod plugins;
pub mod preemption;
pub mod scheduler;
pub mod score;
pub mod spread;
pub mod virtualmachine;
pub mod volumebinding;

pub use scheduler::Scheduler;
