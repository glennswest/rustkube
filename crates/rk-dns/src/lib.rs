//! rk-dns: In-cluster DNS server.
//!
//! Resolves `svc.namespace.svc.cluster.local` using hickory-dns.
//! Watches Services and Endpoints from the API server to maintain
//! A, SRV, and PTR records for cluster services.
