use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::process::Command;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};

use pullrun_exec::types::{Backend, NetworkMode, WorkloadSpec};
use pullrun_exec::{Executor, LinuxContainerExecutor};
use pullrun_oci::{OciPuller, OciToDagConverter};
use pullrun_policy::{CosignKey, Policy};
use pullrun_runtime::binfmt;
use pullrun_runtime::metrics;
use pullrun_runtime::proto;
use pullrun_runtime::service::{RuntimeCommand, RuntimeService, ServiceConfig, VmBackendConfig};
use pullrun_store::{Digest, MmapStore};
use pullrun_sync::{
    BlockSyncClient, BlockSyncServer, BlockSyncService, BloomGossip, Discovery,
    PeerBloomCache, RegistrarClient, RegistrarServer, RegistrarService,
    generate_node_id, run_registrar_client,
};
use proto::runtime_server::RuntimeServer;

#[derive(Parser)]
#[command(name = "pullrun-runtime")]
#[command(about = "Pullrun workload runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the gRPC daemon
    Daemon {
        #[arg(short, long, default_value = "/tmp/pullrun.sock")]
        socket: String,
        #[arg(long, default_value = "/var/lib/pullrun")]
        store_root: PathBuf,

        // Observability.
        //
        // `--metrics-addr` enables a Prometheus `/metrics` endpoint
        // alongside the gRPC UDS. `127.0.0.1:9090` is the default
        // when the flag is passed without a value; bind to
        // `0.0.0.0:9090` to scrape from another host (e.g. Prometheus
        // pod on the same node, or k8s `ServiceMonitor`).
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "127.0.0.1:9090",
            value_name = "ADDR",
            help = "Bind Prometheus /metrics HTTP server to this address (no value = 127.0.0.1:9090)"
        )]
        metrics_addr: Option<SocketAddr>,

        // Policy flags
        #[arg(long, help = "Require cosign signatures on all images")]
        require_signature: bool,
        #[arg(long, help = "Require an SBOM for all images")]
        require_sbom: bool,
        #[arg(long, help = "Maximum allowed CVSS score (0.0-10.0)")]
        max_cvss: Option<f32>,
        #[arg(long, help = "Declare the rootfs must be read-only (declarative)")]
        readonly_rootfs: bool,
        #[arg(long, help = "Forbid privilege escalation (declarative)")]
        no_new_privileges: bool,
        #[arg(long, help = "Banned license identifier (repeatable)")]
        deny_license: Vec<String>,
        #[arg(
            long,
            help = "Trusted cosign public key, base64 (id:base64, repeatable)"
        )]
        trusted_key: Vec<String>,

        // VM backend flags (all optional; VM backend is enabled when
        // --vm-firecracker is set)
        #[arg(
            long,
            help = "Path to the firecracker binary (enables VM backend when set)"
        )]
        vm_firecracker: Option<PathBuf>,
        #[arg(long, help = "Path to a firecracker-compatible vmlinux kernel")]
        vm_kernel: Option<PathBuf>,
        #[arg(long, default_value = "/var/lib/pullrun/vm", help = "Where to store ext4 rootfs images and VM sidecars")]
        vm_root: PathBuf,
        #[arg(long, default_value_t = 2, help = "vCPUs per VM")]
        vm_vcpus: u8,
        #[arg(long, default_value_t = 512, help = "Memory (MiB) per VM")]
        vm_mem_mib: u32,
        #[arg(long, default_value_t = 256, help = "Rootfs size (MiB) per VM")]
        vm_size_mb: u64,

        // OCI pull configuration.
        #[arg(
            long = "insecure-registry",
            help = "Registry to reach over plain HTTP (no TLS). Repeatable. Example: localhost:5000"
        )]
        insecure_registry: Vec<String>,

        // Peer-to-peer block sync. Default is disabled (port 0).
        // Pass e.g. --sync-addr 0.0.0.0:9500 to enable.
        #[arg(
            long = "sync-addr",
            default_value = "0.0.0.0:0",
            help = "TCP address for peer-to-peer BlockSync gRPC (e.g. 0.0.0.0:9500). Port 0 = disabled."
        )]
        sync_addr: std::net::SocketAddr,

        // Registrar (optional centralized peer registry).
        // --registrar-addr: host a registrar on this address.
        // --registrar-connect: register with a remote registrar.
        #[arg(
            long = "registrar-addr",
            help = "Host the Registrar gRPC service on this address (e.g. 0.0.0.0:9600). Not started by default."
        )]
        registrar_addr: Option<SocketAddr>,
        #[arg(
            long = "registrar-connect",
            help = "Register this node with a remote Registrar at this address (e.g. 10.0.0.1:9600)."
        )]
        registrar_connect: Option<SocketAddr>,
    },
    /// Pull an OCI image into the DAG store
    Pull {
        image_ref: String,
        #[arg(long)]
        registry: Option<String>,
        #[arg(long, default_value = "/var/lib/pullrun")]
        store_root: PathBuf,
        #[arg(
            long = "insecure-registry",
            help = "Treat the registry as plain-HTTP (no TLS). Repeatable."
        )]
        insecure_registry: Vec<String>,
    },
    /// Run a workload
    Run {
        /// DAG root digest
        root_digest: String,
        #[arg(long, default_value = "wl-0")]
        name: String,
        #[arg(long, default_value = "container")]
        backend: String,
        #[arg(long)]
        command: Vec<String>,
        #[arg(short, long = "env")]
        env_vars: Vec<String>,
        #[arg(long)]
        allow_outbound: Vec<String>,
        #[arg(long)]
        publish: Vec<u16>,
        #[arg(long, default_value = "/var/lib/pullrun")]
        store_root: PathBuf,
    },
    /// Stop a workload
    Stop {
        id: String,
        #[arg(long, default_value = "/var/lib/pullrun")]
        store_root: PathBuf,
    },
    /// List running workloads
    List {
        #[arg(long, default_value = "/var/lib/pullrun")]
        store_root: PathBuf,
    },
}

fn build_policy(
    require_signature: bool,
    require_sbom: bool,
    max_cvss: Option<f32>,
    readonly_rootfs: bool,
    no_new_privileges: bool,
    deny_license: Vec<String>,
) -> Option<Policy> {
    if !require_signature && !require_sbom && max_cvss.is_none() && !readonly_rootfs
        && !no_new_privileges && deny_license.is_empty()
    {
        return None;
    }
    Some(Policy {
        required_signature: require_signature,
        require_sbom,
        max_cvss_score: max_cvss,
        readonly_rootfs,
        no_new_privileges,
        deny_licenses: deny_license,
        ..Default::default()
    })
}

fn parse_trusted_keys(values: &[String]) -> Vec<CosignKey> {
    let mut out = Vec::new();
    for v in values {
        let (id, b64) = match v.split_once(':') {
            Some(parts) => parts,
            None => {
                warn!("ignoring --trusted-key without `id:base64` form: {v}");
                continue;
            }
        };
        match CosignKey::from_base64(id, b64) {
            Ok(k) => out.push(k),
            Err(e) => warn!("failed to parse --trusted-key {id}: {e}"),
        }
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    // On macOS, the Apple Virtualization framework dispatches its
    // VM completion handlers (startWithCompletionHandler, etc.)
    // onto the **main dispatch queue** that the VM is configured
    // with. The main queue is processed by `dispatch_main()`,
    // which must be called on the **main thread** (it parks
    // the main thread and waits for work on the main queue).
    //
    // This is incompatible with `#[tokio::main]`, which owns
    // the main thread for the async executor. The main thread
    // is not running a libdispatch runloop, so VM completions
    // would never fire and the body thread would block on
    // `recv_timeout` for 30s.
    //
    // Fix: run the daemon in a tokio runtime on a side thread,
    // and call `dispatch_main()` on the main thread. The
    // main thread is "parked" but tokio runs the gRPC server
    // on the side thread and uses `spawn_blocking` to call
    // into the Apple Virt FFI, which dispatches to the main
    // queue.
    //
    // For one-shot subcommands (run, stop, list, pull) we
    // can stay on the main thread because they don't boot
    // Apple Virt VMs. They use a current-thread tokio
    // runtime via `block_on`.

    match cli.command {
        Commands::Daemon { .. } => {
            // See the cfg-gated logic below the match.
            daemon_main(cli)
        }
        other => {
            // One-shot subcommand. Build a current-thread tokio
            // runtime and run the work inline. No Apple Virt
            // FFI happens here, so the main thread can be the
            // tokio thread without any conflict.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_one_shot(other))
        }
    }
}

/// `main()` for the `daemon` subcommand. On macOS this **must**
/// run tokio on a side thread so the main thread can call
/// `dispatch_main()` to pump the main queue (the Apple Virt
/// framework hard-wires VM completion handlers to the main
/// queue). On Linux this can stay on the main thread.
fn daemon_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Move the parsed Cli into a closure so the side thread
    // can take ownership.
    let cli_for_side_thread = cli;

    // Cross-thread channel so the side thread can report
    // fatal errors (e.g. socket bind failure) back to the
    // main thread, which will then exit the process with a
    // non-zero code.
    let (err_tx, _err_rx) = std::sync::mpsc::channel::<String>();

    let _side_thread = std::thread::Builder::new()
        .name("pullrun-runtime-tokio".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            let result: Result<(), Box<dyn std::error::Error>> =
                rt.block_on(run_daemon_cmd(cli_for_side_thread));
            if let Err(e) = result {
                let _ = err_tx.send(format!("{e}"));
            }
            // If the daemon returns Ok(()), the user sent
            // SIGTERM/SIGINT (see run_daemon). The main
            // thread is still in dispatch_main() and will
            // exit when the process does. We exit explicitly
            // so the side thread doesn't outlive the runtime.
            std::process::exit(0);
        })
        .expect("failed to spawn side thread");

    #[cfg(target_os = "macos")]
    {
        // Park the main thread on the main dispatch queue.
        // The Apple Virt framework's XPC plumbing will
        // deliver completion handlers here. We never return
        // from this call. If the side thread hits a fatal
        // error before exit, it will be queued in `err_rx`.
        // We can't easily observe it from this thread, so
        // the side thread calls `std::process::exit(0)` on
        // clean shutdown and the main thread reads the error
        // from the channel on non-macOS platforms.
        // SAFETY: dispatch_main() never returns.
        dispatch2::dispatch_main();
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On Linux, the side thread does the work; the main
        // thread just waits for it. If it errors out, we
        // surface the error.
        drop(_side_thread);
        if let Ok(msg) = _err_rx.recv() {
            return Err(msg.into());
        }
        Ok(())
    }
}

async fn run_one_shot(cmd: Commands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Commands::Pull { image_ref, registry, store_root, insecure_registry } => {
            run_pull(
                &image_ref,
                registry.as_deref(),
                &store_root,
                &insecure_registry,
            )
            .await?;
        }
        Commands::Run {
            root_digest,
            name,
            backend,
            command,
            env_vars,
            allow_outbound,
            publish,
            store_root,
        } => {
            run_workload(
                &root_digest,
                &name,
                &backend,
                &command,
                &env_vars,
                &allow_outbound,
                &publish,
                &store_root,
            )
            .await?;
        }
        Commands::Stop { id, store_root } => run_stop(&id, &store_root).await?,
        Commands::List { store_root } => run_list(&store_root).await?,
        Commands::Daemon { .. } => unreachable!("daemon commands are dispatched via daemon_main"),
    }
    Ok(())
}

async fn run_daemon_cmd(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let Commands::Daemon {
        socket,
        store_root,
        metrics_addr,
        require_signature,
        require_sbom,
        max_cvss,
        readonly_rootfs,
        no_new_privileges,
        deny_license,
        trusted_key,
        vm_firecracker,
        vm_kernel,
        vm_root,
        vm_vcpus,
        vm_mem_mib,
        vm_size_mb,
        insecure_registry,
        sync_addr,
        registrar_addr,
        registrar_connect,
    } = cli.command
    else {
        unreachable!("daemon_main only passes the Daemon variant")
    };
    run_daemon(
        &socket,
        store_root,
        metrics_addr,
        require_signature,
        require_sbom,
        max_cvss,
        readonly_rootfs,
        no_new_privileges,
        deny_license,
        trusted_key,
        vm_firecracker,
        vm_kernel,
        vm_root,
        vm_vcpus,
        vm_mem_mib,
        vm_size_mb,
        insecure_registry,
        sync_addr,
        registrar_addr,
        registrar_connect,
    )
    .await
}



async fn run_pull(
    image_ref: &str,
    registry: Option<&str>,
    store_root: &std::path::Path,
    insecure_registries: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(MmapStore::new(store_root.to_path_buf()));
    let puller = OciPuller::with_insecure_registries(
        None,
        insecure_registries.iter().cloned().collect(),
    );
    let pulled = puller.pull(image_ref, registry).await?;
    let converter = OciToDagConverter::new(store.clone());
    let root_digest = converter.convert(&pulled).await?;

    println!("Root: {root_digest}");
    println!("Layers stored: {}", pulled.layer_blobs.len());
    println!("Bytes deduplicated: 0");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_workload(
    root_digest: &str,
    name: &str,
    backend: &str,
    command: &[String],
    env_vars: &[String],
    _allow_outbound: &[String],
    _publish: &[u16],
    store_root: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = MmapStore::new(store_root.to_path_buf());
    let bundle_root = store_root.join("bundles");
    std::fs::create_dir_all(&bundle_root)?;

    let executor = LinuxContainerExecutor::new(store, None, bundle_root);

    let backend = Backend::from_str(backend).map_err(|e| e.to_string())?;
    let env: HashMap<String, String> = env_vars
        .iter()
        .filter_map(|e| {
            let mut parts = e.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
        })
        .collect();

    let cmd = if command.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        command.to_vec()
    };

    let image_root = Digest::from_hex(root_digest)
        .map_err(|e| format!("invalid digest: {e}"))?;
    let spec = WorkloadSpec {
        id: name.to_string(),
        image_root,
        backend,
        command: cmd.clone(),
        env,
        cpu_millicores: None,
        memory_bytes: None,
        network_mode: NetworkMode::Loopback,
        network_rules: vec![],
        kernel_path: None,
        bridge_name: None,
        mounts: vec![],
        health_check: None,
        restart_policy: Default::default(),
    };

    let handle = executor.create(spec).await?;
    executor.start(&handle).await?;

    println!(
        "Started {} (pid {:?}, backend: {}, network: loopback)",
        name, handle.pid, handle.backend
    );
    Ok(())
}

async fn run_stop(id: &str, store_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = MmapStore::new(store_root.to_path_buf());
    let bundle_root = store_root.join("bundles");
    let executor = LinuxContainerExecutor::new(store, None, bundle_root);
    executor.stop(id).await?;
    println!("Stopped {id}");
    Ok(())
}

async fn run_list(_store_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("runc").args(["list"]).output().await?;
    println!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_daemon(
    socket: &str,
    store_root: PathBuf,
    metrics_addr: Option<SocketAddr>,
    require_signature: bool,
    require_sbom: bool,
    max_cvss: Option<f32>,
    readonly_rootfs: bool,
    no_new_privileges: bool,
    deny_license: Vec<String>,
    trusted_key: Vec<String>,
    vm_firecracker: Option<PathBuf>,
    vm_kernel: Option<PathBuf>,
    vm_root: PathBuf,
    vm_vcpus: u8,
    vm_mem_mib: u32,
    vm_size_mb: u64,
    insecure_registry: Vec<String>,
    sync_addr: SocketAddr,
    registrar_addr: Option<SocketAddr>,
    registrar_connect: Option<SocketAddr>,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = build_policy(
        require_signature,
        require_sbom,
        max_cvss,
        readonly_rootfs,
        no_new_privileges,
        deny_license,
    );
    let keys = parse_trusted_keys(&trusted_key);
    let mut config = ServiceConfig::new(store_root).trusted_keys(keys);
    if let Some(p) = policy {
        config = config.with_policy(p);
    }
    if !insecure_registry.is_empty() {
        info!(
            registries = ?insecure_registry,
            "insecure (plain-HTTP) OCI registries configured"
        );
        config = config.with_insecure_registries(insecure_registry.into_iter().collect());
    }
    if let (Some(fc), Some(kernel)) = (vm_firecracker, vm_kernel) {
        config = config.with_vm_backend(VmBackendConfig {
            firecracker_path: fc,
            kernel_path: kernel,
            vm_root,
            vcpus: vm_vcpus,
            mem_mib: vm_mem_mib,
            size_mb: vm_size_mb,
        });
    }
    info!(%socket, store_root = %config.store_root.display(), "starting pullrun-runtime daemon");

    // Install the Prometheus recorder *before* constructing the
    // service so the `record_*` calls inside pull_image / run_workload
    // hit a real recorder, not a no-op. Also start the metrics HTTP
    // server here if requested — the gRPC UDS is bound after, but the
    // operator may want to scrape /metrics *before* the gRPC server
    // is up (e.g. to verify the daemon started at all).
    let metrics_handle = metrics::install_recorder();
    if let Some(addr) = metrics_addr {
        let h = metrics_handle.clone();
        tokio::spawn(async move { metrics::serve(addr, h).await });
    } else {
        info!(
            "metrics endpoint disabled (pass --metrics-addr to enable; e.g. --metrics-addr or --metrics-addr=0.0.0.0:9090)"
        );
    }

    // Register binfmt_misc handlers for common architectures so
    // cross-arch container execution works without manual setup.
    binfmt::register_common_binfmts();

    // Build the store early so BlockSync and Discovery can share it.
    let store = Arc::new(MmapStore::new(config.store_root.clone()));

    // Clean up any leftover secrets-stage directories from a previous
    // crash or unclean shutdown.
    let staged_root = config.store_root.join("run").join("secrets-stage");
    if staged_root.exists() {
        std::fs::remove_dir_all(&staged_root).ok();
        info!("cleaned up stale secrets-stage directory");
    }

    // Generate a single node ID for the lifetime of this daemon.
    // Used for both mDNS discovery and registrar registration.
    let node_id = generate_node_id();
    info!(%node_id, "node identity");

    // Start peer-to-peer BlockSync gRPC server (enabled when port != 0).
    let bloom_cache: Option<PeerBloomCache> = if sync_addr.port() != 0 {
        let block_sync_service = BlockSyncService::new(store.clone());
        let bss = BlockSyncServer::new(block_sync_service.clone());
        let addr = sync_addr;
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(bss)
                .serve(addr)
                .await
                .ok();
        });
        info!(sync_addr = %sync_addr, "BlockSync gRPC server started");

        // Build BlockSync client (connected to our own server for gossip).
        let local_client = BlockSyncClient::connect(format!("http://{}", sync_addr))
            .await
            .ok();
        if local_client.is_some() {
            info!("BlockSync client initialized");
        }

        // Start mDNS peer discovery.
        let discovery = Discovery::new(node_id.clone(), sync_addr);
        let dh = discovery.clone();
        tokio::spawn(async move {
            dh.run().await;
        });

        // Shared bloom cache for gossip exchange.
        let cache = PeerBloomCache::new();

        // Start bloom filter gossip (periodic bloom filter exchange with peers).
        if let Some(client) = local_client {
            let gossip = BloomGossip::new(client, block_sync_service.clone(), discovery.clone(), cache.clone());
            tokio::spawn(async move {
                gossip.run().await;
            });
            info!("bloom filter gossip started");
        }

        // Periodically rebuild bloom filter from store contents.
        let bf = block_sync_service.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            bf.rebuild_bloom_filter().await;
            info!(blob_count = bf.blob_count(), "bloom filter rebuilt");
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                bf.rebuild_bloom_filter().await;
            }
        });

        Some(cache)
    } else {
        info!("BlockSync disabled (--sync-addr port is 0)");
        None
    };

    if let Some(cache) = &bloom_cache {
        config = config.with_bloom_cache(cache.clone());
        info!("PeerBloomCache wired into service config");
    }

    // ─── Registrar (optional centralized peer registry) ─────────
    if let Some(addr) = registrar_addr {
        let reg_svc = RegistrarService::new();
        let reg_svr = RegistrarServer::new(reg_svc.clone());
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(reg_svr)
                .serve(addr)
                .await
                .ok();
        });
        info!(registrar_addr = %addr, "Registrar gRPC server started");

        // Background eviction of stale peers.
        tokio::spawn(async move {
            reg_svc.run_eviction().await;
        });
    }

    // Register with a remote registrar if --registrar-connect is set.
    if let Some(connect_addr) = registrar_connect {
        let sync_addr_str = if sync_addr.port() != 0 {
            sync_addr.to_string()
        } else {
            warn!("--registrar-connect set but block sync is disabled (--sync-addr port 0); peers cannot connect to this node");
            String::new()
        };
        match RegistrarClient::connect(format!("http://{}", connect_addr)).await {
            Ok(client) => {
                let node_id_c = node_id.clone();
                let sync_addr_c = sync_addr_str.clone();
                tokio::spawn(async move {
                    run_registrar_client(client, node_id_c, sync_addr_c).await;
                });
                info!(%node_id, registrar = %connect_addr, "registered with remote registrar");
            }
            Err(e) => {
                warn!(error = %e, registrar = %connect_addr, "failed to connect to registrar");
            }
        }
    }

    if std::fs::metadata(socket).is_ok() {
        std::fs::remove_file(socket)?;
    }

    let cmd = RuntimeCommand::new(config);
    let service: RuntimeService = cmd.service();

    // Spawn the periodic gauge updater for store stats. We sample
    // every 60s; the gauges are not on the hot path so a coarse
    // interval is fine. If the runtime is short-lived (CLI mode), this
    // task is never started, so no cost.
    {
        let store = service.store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let nodes = store.cached_node_count();
                let bytes = store.total_bytes();
                metrics::record_store_stats(nodes, bytes);
            }
        });
    }

    // Spawn a gauge updater for block sync metrics (peer count).
    if let Some(cache) = &bloom_cache {
        let bc = cache.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let n = bc.peer_count().await;
                metrics::record_sync_peer_count(n);
            }
        });
    }

    let uds = tokio::net::UnixListener::bind(socket)?;
    info!(%socket, "listening on Unix Domain Socket");

    let uds_stream = UnixListenerStream::new(uds);
    let svc = RuntimeServer::new(service);

    Server::builder()
        .add_service(svc)
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}
