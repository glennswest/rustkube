//! rk-dns: In-cluster DNS server.
//!
//! Resolves `svc.namespace.svc.cluster.local` using hickory-dns.
//! Watches Services and Endpoints from the API server to maintain
//! A, SRV, and PTR records for cluster services.

pub mod authority;
pub mod records;
pub mod server;
pub mod syncer;

pub use server::ClusterDns;
