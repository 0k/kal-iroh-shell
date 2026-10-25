//! `kal-iroh-shell-releases`: serve one app's published interface
//! releases and push "new release" events.
//!
//! Meant to sit behind a reverse proxy that carries it to the shells
//! (for example a vigil-watch serve mount, reached over Iroh).  See the
//! `kal_iroh_shell_server` library.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// Serve the interface releases published in DIR and their control stream.
///
/// Example: kal-iroh-shell-releases --dir ~/.local/share/my-app-releases
#[derive(Parser)]
#[command(name = "kal-iroh-shell-releases", version, about)]
struct Args {
    /// Publish directory (holds `current.json` and one directory per release)
    #[arg(long, env = "KAL_IROH_SHELL_RELEASES_DIR")]
    dir: PathBuf,

    /// Address to listen on
    #[arg(
        long,
        env = "KAL_IROH_SHELL_RELEASES_LISTEN",
        default_value = "127.0.0.1:4310"
    )]
    listen: SocketAddr,

    /// -v / -l [OUTPUT/]TARGET:LEVEL / --log-out OUTPUT=DEST / --log-time
    #[command(flatten)]
    logging: kal_log::LogArgs,
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    let _log =
        kal_log::setup(&kal_log::LogConfig::default(), &args.logging).map_err(|e| e.to_string())?;
    if !args.dir.is_dir() {
        return Err(format!("{} is not a directory", args.dir.display()));
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let server = kal_iroh_shell_server::ReleaseServer::new(args.dir.clone());
        let _watcher = server
            .watch()
            .map_err(|e| format!("watch {}: {e}", args.dir.display()))?;
        let listener = tokio::net::TcpListener::bind(args.listen)
            .await
            .map_err(|e| format!("listen {}: {e}", args.listen))?;
        log::info!("serving {} on http://{}", args.dir.display(), args.listen);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| e.to_string())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
