//! ClickFS CLI entrypoint.

mod driver;
mod error;
#[cfg(feature = "fuse")]
mod fs;
mod inode;
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

    use crate::driver::HttpDriver;
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

            /// Auto-unmount when the process exits.
            #[arg(long, default_value_t = true)]
            auto_unmount: bool,
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
                auto_unmount,
            } => run_mount(
                url,
                mountpoint,
                user,
                password,
                query_timeout,
                max_result_bytes,
                allow_other,
                auto_unmount,
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
        auto_unmount: bool,
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

        let fs = ClickFs::new(driver, rt.handle().clone());

        let mut options = vec![MountOption::FSName("clickfs".to_string()), MountOption::RO];
        // These options are Linux-only (libfuse / fusermount). macFUSE rejects them
        // with EINPROGRESS at mount time.
        #[cfg(target_os = "linux")]
        {
            options.push(MountOption::Subtype("clickfs".to_string()));
            options.push(MountOption::DefaultPermissions);
            options.push(MountOption::NoSuid);
            options.push(MountOption::NoDev);
            if auto_unmount {
                options.push(MountOption::AutoUnmount);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = auto_unmount; // silence unused-variable warning on macOS
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
