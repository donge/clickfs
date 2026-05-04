//! ClickFS CLI entrypoint.

mod cache;
mod driver;
mod error;
#[cfg(feature = "fuse")]
mod fs;
mod inode;
mod readme;
mod resolver;
mod stream;

#[cfg(not(feature = "fuse"))]
fn main() {
    eprintln!("clickfs was built without the `fuse` feature; mount/umount unavailable.");
    eprintln!("Rebuild with `--features fuse` (default) on Linux or macOS with macFUSE.");
    std::process::exit(1);
}

#[cfg(feature = "fuse")]
mod cli {
    use std::path::PathBuf;

    use clap::{Parser, Subcommand};
    use fuser::MountOption;
    use tokio::runtime::Runtime;
    use tracing_subscriber::EnvFilter;
    use url::Url;

    use crate::driver::{CompressionConfig, HttpDriver, TailConfig, TlsConfig};
    use crate::fs::ClickFs;

    #[derive(Parser, Debug)]
    #[command(version, about = "Mount ClickHouse as a filesystem", long_about = None)]
    struct Cli {
        #[command(subcommand)]
        cmd: Command,
    }

    #[derive(Subcommand, Debug)]
    enum Command {
        /// Mount a ClickHouse server at the given mountpoint.
        Mount {
            /// ClickHouse HTTP endpoint, e.g. http://localhost:8123
            url: String,

            /// Local mountpoint directory (must exist).
            mountpoint: PathBuf,

            /// ClickHouse user.
            #[arg(long, env = "CLICKFS_USER", default_value = "default")]
            user: String,

            /// ClickHouse password (prefer env var).
            #[arg(long, env = "CLICKFS_PASSWORD", default_value = "")]
            password: String,

            /// Per-query max execution time, seconds.
            #[arg(long, default_value_t = 60)]
            query_timeout: u64,

            /// Per-query max result size in bytes.
            #[arg(long, default_value_t = 1_073_741_824u64)]
            max_result_bytes: u64,

            /// Allow other users to access the mount.
            #[arg(long, default_value_t = false)]
            allow_other: bool,

            /// Don't auto-unmount when the process exits (mount survives
            /// process death; unmount manually with `clickfs umount`).
            #[arg(long, default_value_t = false)]
            no_auto_unmount: bool,

            /// Disable TLS certificate verification (dev/lab only).
            /// Mutually exclusive with --ca-bundle.
            #[arg(long, default_value_t = false, conflicts_with = "ca_bundle")]
            insecure: bool,

            /// Path to a PEM file containing extra CA certificate(s) to
            /// trust on top of the system trust store. Mutually exclusive
            /// with --insecure.
            #[arg(long, value_name = "PATH")]
            ca_bundle: Option<PathBuf>,

            /// Disable HTTP gzip compression. By default clickfs sends
            /// `Accept-Encoding: gzip` and asks ClickHouse to compress
            /// the response (`enable_http_compression=1`), which is
            /// typically a 30-50% speedup over WAN/TLS for large
            /// streams. The flag is mostly useful when debugging the
            /// raw SQL response in mount.log.
            #[arg(long, default_value_t = false)]
            no_compression: bool,

            /// Disable tail-mode reverse-pread synthesis. By default a
            /// large reverse pread (e.g. `tail -n 10 all.tsv`) triggers
            /// a one-shot `SELECT * ORDER BY <pk> DESC LIMIT N` that is
            /// served as a buffer pinned to the file's pseudo-EOF, so
            /// the kernel `tail` works natively. With this flag any
            /// reverse pread returns EIO (the v0.2 behavior).
            #[arg(long, default_value_t = false)]
            no_tail: bool,

            /// Hard cap on tail-buffer rows (per open file handle).
            /// The buffer expands on demand through 10 → 100 → 1000 →
            /// up to this cap, so `tail file` only pays for 10 rows
            /// while `tail -n 10000` walks the full ladder. Values are
            /// clamped to 1..=10000: an AI agent reading the buffer
            /// should never see more than ~10 MB at typical row sizes.
            #[arg(long, env = "CLICKFS_TAIL_ROWS", default_value_t = 10_000)]
            tail_rows: u32,
        },

        /// Unmount a previously mounted clickfs.
        Umount { mountpoint: PathBuf },
    }

    pub fn run() -> std::process::ExitCode {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("clickfs=info,fuser=warn"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();

        let cli = Cli::parse();

        match cli.cmd {
            Command::Mount {
                url,
                mountpoint,
                user,
                password,
                query_timeout,
                max_result_bytes,
                allow_other,
                no_auto_unmount,
                insecure,
                ca_bundle,
                no_compression,
                no_tail,
                tail_rows,
            } => run_mount(
                url,
                mountpoint,
                user,
                password,
                query_timeout,
                max_result_bytes,
                allow_other,
                no_auto_unmount,
                insecure,
                ca_bundle,
                no_compression,
                no_tail,
                tail_rows,
            ),
            Command::Umount { mountpoint } => run_umount(mountpoint),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_mount(
        url: String,
        mountpoint: PathBuf,
        user: String,
        password: String,
        query_timeout: u64,
        max_result_bytes: u64,
        allow_other: bool,
        no_auto_unmount: bool,
        insecure: bool,
        ca_bundle: Option<PathBuf>,
        no_compression: bool,
        no_tail: bool,
        tail_rows: u32,
    ) -> std::process::ExitCode {
        let parsed_url = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("clickfs: invalid url '{}': {}", url, e);
                return std::process::ExitCode::from(2);
            }
        };

        if !mountpoint.is_dir() {
            eprintln!(
                "clickfs: mountpoint '{}' does not exist or is not a directory",
                mountpoint.display()
            );
            return std::process::ExitCode::from(2);
        }

        let ca_bundle_pem = match ca_bundle.as_ref() {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    eprintln!(
                        "clickfs: failed to read --ca-bundle '{}': {}",
                        path.display(),
                        e
                    );
                    return std::process::ExitCode::from(2);
                }
            },
            None => None,
        };
        let tls = TlsConfig {
            insecure,
            ca_bundle_pem,
        };
        let compression = CompressionConfig {
            enabled: !no_compression,
        };
        let tail_cfg = TailConfig {
            enabled: !no_tail,
            rows: tail_rows.clamp(1, 10_000),
        };

        let rt = match Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("clickfs: failed to start tokio runtime: {}", e);
                return std::process::ExitCode::from(1);
            }
        };

        let driver = match HttpDriver::new(
            parsed_url.clone(),
            user.clone(),
            password,
            query_timeout,
            max_result_bytes,
            tls,
            compression,
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("clickfs: failed to build driver: {}", e);
                return std::process::ExitCode::from(1);
            }
        };

        let driver_check = driver.clone();
        if let Err(e) = rt.block_on(async move { driver_check.ping().await }) {
            eprintln!(
                "clickfs: failed to connect to clickhouse at {}: {}",
                parsed_url, e
            );
            return std::process::ExitCode::from(1);
        }
        tracing::info!(url = %parsed_url, %user, "connected to clickhouse");

        let fs = ClickFs::new(driver, rt.handle().clone(), tail_cfg);

        let mut options = vec![MountOption::FSName("clickfs".to_string()), MountOption::RO];
        // These options are Linux-only (libfuse / fusermount). macFUSE rejects them
        // with EINPROGRESS at mount time.
        #[cfg(target_os = "linux")]
        {
            options.push(MountOption::Subtype("clickfs".to_string()));
            options.push(MountOption::DefaultPermissions);
            options.push(MountOption::NoSuid);
            options.push(MountOption::NoDev);
            if !no_auto_unmount {
                options.push(MountOption::AutoUnmount);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = no_auto_unmount; // silence unused-variable warning on macOS
        }
        if allow_other {
            options.push(MountOption::AllowOther);
        }

        let mp_for_signal = mountpoint.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => return,
            };
            rt.block_on(async move {
                let mut sigterm =
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
                tracing::info!("received shutdown signal, unmounting");
                let _ = std::process::Command::new("fusermount")
                    .arg("-u")
                    .arg(&mp_for_signal)
                    .status();
                let _ = std::process::Command::new("umount")
                    .arg(&mp_for_signal)
                    .status();
            });
        });

        tracing::info!(mountpoint = %mountpoint.display(), "mounting");
        eprintln!(
            "clickfs: mounted at {} (Ctrl-C to unmount)",
            mountpoint.display()
        );

        if let Err(e) = fuser::mount2(fs, &mountpoint, &options) {
            eprintln!("clickfs: mount failed: {}", e);
            return std::process::ExitCode::from(1);
        }

        tracing::info!("filesystem session ended");
        std::process::ExitCode::SUCCESS
    }

    fn run_umount(mountpoint: PathBuf) -> std::process::ExitCode {
        let status = if cfg!(target_os = "linux") {
            std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&mountpoint)
                .status()
        } else {
            std::process::Command::new("umount")
                .arg(&mountpoint)
                .status()
        };
        match status {
            Ok(s) if s.success() => std::process::ExitCode::SUCCESS,
            Ok(s) => std::process::ExitCode::from(s.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!("clickfs: umount failed: {}", e);
                std::process::ExitCode::from(1)
            }
        }
    }
} // end mod cli

#[cfg(feature = "fuse")]
fn main() -> std::process::ExitCode {
    cli::run()
}
