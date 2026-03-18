//! rustkube — single-binary Kubernetes cluster.
//!
//! Runs all components in one process:
//! - API server (K8s REST API)
//! - Controller manager (Deployment, ReplicaSet, Service, Namespace, Node)
//! - Scheduler (filter/score pod placement)
//! - Kubelet (CRI pod lifecycle on this node)
//! - Service proxy (iptables DNAT for ClusterIP/NodePort)
//! - Cluster DNS (hickory-dns for svc.cluster.local)

use clap::Parser;
use rk_apiserver::ApiServerConfig;
use rk_controllers::ControllerManager;
use rk_dns::{ClusterDns, server::DnsConfig};
use rk_kubelet::{
    CriClient, Kubelet, KubeletConfig, NativeRuntime, NativeImageService,
    VmRuntime, VmmBackend, detect_cri_socket,
};
use rk_proxy::{ServiceProxy, proxy::ProxyConfig};
use rk_scheduler::Scheduler;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rustkube", about = "RustKube — K8s-compatible orchestrator (all-in-one)")]
struct Cli {
    /// Bind address for API server
    #[arg(long, default_value = "0.0.0.0")]
    bind_addr: String,

    /// API server port
    #[arg(long, default_value_t = 6443)]
    secure_port: u16,

    /// Data directory for embedded store
    #[arg(long, default_value = "/var/lib/rustkube")]
    data_dir: PathBuf,

    /// Service CIDR
    #[arg(long, default_value = "10.96.0.0/12")]
    service_cidr: String,

    /// Pod CIDR for this node
    #[arg(long, default_value = "10.244.0.0/24")]
    pod_cidr: String,

    /// Cluster domain
    #[arg(long, default_value = "cluster.local")]
    cluster_domain: String,

    /// Node name (defaults to hostname)
    #[arg(long, env = "NODE_NAME")]
    node_name: Option<String>,

    /// CRI socket path (only used with --runtime=cri)
    #[arg(long, env = "CRI_SOCKET")]
    cri_socket: Option<String>,

    /// Container runtime: native (libcontainer), vm (microVM), cri (external CRI)
    #[arg(long, default_value = "native", value_parser = ["native", "vm", "cri"])]
    runtime: String,

    /// VMM backend for --runtime=vm: cloud-hypervisor, qemu, firecracker, auto
    #[arg(long, default_value = "auto", value_parser = ["auto", "cloud-hypervisor", "qemu", "firecracker"])]
    vmm: String,

    /// DNS listen port
    #[arg(long, default_value_t = 10053)]
    dns_port: u16,

    /// Disable kubelet (control plane only)
    #[arg(long)]
    no_kubelet: bool,

    /// Disable proxy
    #[arg(long)]
    no_proxy: bool,

    /// Disable DNS
    #[arg(long)]
    no_dns: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let internal_url = format!("http://127.0.0.1:{}", cli.secure_port);

    let node_name = cli.node_name.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("NODE_NAME"))
            .unwrap_or_else(|_| gethostname())
    });

    tracing::info!("RustKube starting — node={node_name}");

    // ── API Server ──
    let config = ApiServerConfig {
        bind_addr: cli.bind_addr.clone(),
        secure_port: cli.secure_port,
        data_dir: cli.data_dir,
        service_cidr: cli.service_cidr,
        cluster_domain: cli.cluster_domain.clone(),
        ..Default::default()
    };

    let api_handle = tokio::spawn(async move {
        if let Err(e) = rk_apiserver::run(config).await {
            tracing::error!("API server failed: {e}");
        }
    });

    // Wait for API server to be ready
    wait_for_api(&internal_url).await;

    // ── Controller Manager ──
    let cm_url = internal_url.clone();
    let cm_handle = tokio::spawn(async move {
        let cm = ControllerManager::new(&cm_url);
        if let Err(e) = cm.run().await {
            tracing::error!("Controller manager failed: {e}");
        }
    });

    // ── Scheduler ──
    let sched_url = internal_url.clone();
    let sched_handle = tokio::spawn(async move {
        let sched = Scheduler::new(&sched_url);
        if let Err(e) = sched.run().await {
            tracing::error!("Scheduler failed: {e}");
        }
    });

    // ── Kubelet ──
    let kubelet_handle = if !cli.no_kubelet {
        let kubelet_url = internal_url.clone();
        let kubelet_node = node_name.clone();
        let runtime_type = cli.runtime.clone();
        let vmm_backend = cli.vmm.clone();
        let cri_socket_opt = cli.cri_socket.clone();
        Some(tokio::spawn(async move {
            let (runtime, images, migration): (
                Arc<dyn rk_kubelet::cri::RuntimeService>,
                Arc<dyn rk_kubelet::cri::ImageService>,
                Arc<dyn rk_kubelet::cri::MigrationService>,
            ) = match runtime_type.as_str() {
                "vm" => {
                    let backend = match vmm_backend.as_str() {
                        "cloud-hypervisor" => Some(VmmBackend::CloudHypervisor),
                        "qemu" => Some(VmmBackend::Qemu),
                        "firecracker" => Some(VmmBackend::Firecracker),
                        _ => VmmBackend::detect(),
                    };

                    if let Some(backend) = backend {
                        tracing::info!("Kubelet using VM runtime ({:?})", backend);
                        let rt = Arc::new(VmRuntime::new(backend));
                        let img = Arc::new(NativeImageService::new());
                        let mig = rt.clone() as Arc<dyn rk_kubelet::cri::MigrationService>;
                        (rt as _, img as _, mig)
                    } else {
                        tracing::error!("No VMM found, falling back to native runtime");
                        let rt = Arc::new(NativeRuntime::new());
                        let img = Arc::new(NativeImageService::new());
                        let mig = rt.clone() as Arc<dyn rk_kubelet::cri::MigrationService>;
                        (rt as _, img as _, mig)
                    }
                }
                "cri" => {
                    let socket = cri_socket_opt.unwrap_or_else(detect_cri_socket);
                    tracing::info!("Kubelet using CRI runtime ({})", socket);
                    let rt = Arc::new(CriClient::new(&socket));
                    let img = Arc::new(CriClient::new(&socket));
                    let mig = rt.clone() as Arc<dyn rk_kubelet::cri::MigrationService>;
                    (rt as _, img as _, mig)
                }
                _ => {
                    // "native" — default
                    tracing::info!("Kubelet using native runtime (libcontainer)");
                    let rt = Arc::new(NativeRuntime::new());
                    let img = Arc::new(NativeImageService::new());
                    let mig = rt.clone() as Arc<dyn rk_kubelet::cri::MigrationService>;
                    (rt as _, img as _, mig)
                }
            };

            let config = KubeletConfig {
                node_name: kubelet_node,
                api_server_url: kubelet_url,
                ..Default::default()
            };

            let kubelet = Kubelet::new(config, runtime, images, migration);
            if let Err(e) = kubelet.run().await {
                tracing::error!("Kubelet failed: {e}");
            }
        }))
    } else {
        None
    };

    // ── Service Proxy ──
    let proxy_handle = if !cli.no_proxy {
        let proxy_url = internal_url.clone();
        let proxy_node = node_name.clone();
        Some(tokio::spawn(async move {
            let config = ProxyConfig {
                api_server_url: proxy_url,
                node_name: proxy_node,
                ..Default::default()
            };
            let proxy = ServiceProxy::new(config);
            if let Err(e) = proxy.run().await {
                tracing::error!("Service proxy failed: {e}");
            }
        }))
    } else {
        None
    };

    // ── Cluster DNS ──
    let dns_handle = if !cli.no_dns {
        let dns_url = internal_url.clone();
        let dns_domain = cli.cluster_domain;
        let dns_port = cli.dns_port;
        Some(tokio::spawn(async move {
            let config = DnsConfig {
                listen_addr: "0.0.0.0".into(),
                listen_port: dns_port,
                api_server_url: dns_url,
                cluster_domain: dns_domain,
                ..Default::default()
            };
            let dns = ClusterDns::new(config);
            if let Err(e) = dns.run().await {
                tracing::error!("Cluster DNS failed: {e}");
            }
        }))
    } else {
        None
    };

    tracing::info!("RustKube cluster running on :{}", cli.secure_port);
    tracing::info!("  API server:  http://{}:{}", cli.bind_addr, cli.secure_port);
    if kubelet_handle.is_some() {
        tracing::info!("  Kubelet:     node={node_name} runtime={}", cli.runtime);
    }
    if proxy_handle.is_some() {
        tracing::info!("  Proxy:       iptables mode");
    }
    if dns_handle.is_some() {
        tracing::info!("  DNS:         :{}", cli.dns_port);
    }

    // Wait for any component to exit
    tokio::select! {
        r = api_handle => { tracing::error!("API server exited: {r:?}"); }
        r = cm_handle => { tracing::error!("Controller manager exited: {r:?}"); }
        r = sched_handle => { tracing::error!("Scheduler exited: {r:?}"); }
        r = async {
            if let Some(h) = kubelet_handle { h.await } else { std::future::pending().await }
        } => { tracing::error!("Kubelet exited: {r:?}"); }
        r = async {
            if let Some(h) = proxy_handle { h.await } else { std::future::pending().await }
        } => { tracing::error!("Proxy exited: {r:?}"); }
        r = async {
            if let Some(h) = dns_handle { h.await } else { std::future::pending().await }
        } => { tracing::error!("DNS exited: {r:?}"); }
    }

    Ok(())
}

/// Wait for the API server to respond.
async fn wait_for_api(url: &str) {
    let client = reqwest::Client::new();
    let healthz = format!("{url}/healthz");

    for i in 0..30 {
        match client.get(&healthz).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("API server ready");
                return;
            }
            _ => {
                if i == 0 {
                    tracing::info!("Waiting for API server...");
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
    }
    tracing::warn!("API server not responding after 15s, starting components anyway");
}

fn gethostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        unsafe {
            if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                return String::from_utf8_lossy(&buf[..len]).to_string();
            }
        }
    }
    "localhost".to_string()
}
