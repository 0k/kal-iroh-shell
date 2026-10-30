//! `kal-iroh-shell-releases`: manage an app's release store (publish,
//! promote, inspect channels) and serve it to the shells.
//!
//! The store is a bare git repository: a release is a commit of a built
//! interface, a channel (`dev`, `alpha`, `beta`, `stable`, …) is a branch.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use kal_iroh_shell_server::{REFRESH_FALLBACK, ReleaseServer, Store};

/// Manage and serve an app's interface releases.
///
/// Examples:
///   kal-iroh-shell-releases --store ~/.local/share/my-app-releases.git publish --channel dev frontend/dist
///   kal-iroh-shell-releases --store … promote dev alpha
///   kal-iroh-shell-releases --store … promote stable~1 stable   (undo)
///   kal-iroh-shell-releases --store … channels
///   kal-iroh-shell-releases --store … serve
#[derive(Parser)]
#[command(name = "kal-iroh-shell-releases", version, about, verbatim_doc_comment)]
struct Args {
    /// Release store (bare git repository; created by the first `publish`)
    #[arg(long, env = "KAL_IROH_SHELL_RELEASES_STORE")]
    store: PathBuf,

    /// -v / -l [OUTPUT/]TARGET:LEVEL / --log-out OUTPUT=DEST / --log-time
    #[command(flatten)]
    logging: kal_log::LogArgs,

    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Serve the channels, their releases and the control stream
    Serve {
        /// Address to listen on
        #[arg(
            long,
            env = "KAL_IROH_SHELL_RELEASES_LISTEN",
            default_value = "127.0.0.1:4310"
        )]
        listen: SocketAddr,
    },
    /// Commit a finished build as a new release on top of CHANNEL
    Publish {
        /// Channel to publish to
        #[arg(long, default_value = "dev")]
        channel: String,
        /// What the build was made from (recorded in the release)
        #[arg(long, default_value = "unknown")]
        source: String,
        /// The build directory (holds `ui-manifest.json`)
        dist: PathBuf,
    },
    /// Move CHANNEL to the release TARGET names (channel, revision, build id)
    Promote { target: String, channel: String },
    /// Where every channel points
    Channels,
    /// History of a channel, newest first
    Log {
        channel: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        max_count: usize,
    },
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    let _log =
        kal_log::setup(&kal_log::LogConfig::default(), &args.logging).map_err(|e| e.to_string())?;
    let open = || Store::open(&args.store).map_err(|e| e.to_string());
    match args.action {
        Action::Publish {
            channel,
            source,
            dist,
        } => {
            let store = Store::open_or_init(&args.store).map_err(|e| e.to_string())?;
            let commit = store
                .publish(&channel, &dist, &source)
                .map_err(|e| e.to_string())?;
            let build = store.manifest(&commit).map_err(|e| e.to_string())?.build_id;
            println!("{channel}\t{build}\t{commit}");
            log::info!("published release {build} to channel {channel}");
            Ok(())
        }
        Action::Promote { target, channel } => {
            let store = open()?;
            let commit = store
                .promote(&target, &channel)
                .map_err(|e| e.to_string())?;
            let build = store.manifest(&commit).map_err(|e| e.to_string())?.build_id;
            println!("{channel}\t{build}\t{commit}");
            Ok(())
        }
        Action::Channels => {
            for h in open()?.channels().map_err(|e| e.to_string())? {
                println!(
                    "{}\t{}\t{}\t{}",
                    h.channel,
                    h.build_id,
                    h.published_at,
                    &h.commit[..12]
                );
            }
            Ok(())
        }
        Action::Log { channel, max_count } => {
            for e in open()?
                .log(&channel, max_count)
                .map_err(|e| e.to_string())?
            {
                println!("{}\t{}\t{}", &e.commit[..12], e.published_at, e.build_id);
            }
            Ok(())
        }
        Action::Serve { listen } => serve(open()?, listen),
    }
}

fn serve(store: Store, listen: SocketAddr) -> Result<(), String> {
    let dir = store.dir().to_path_buf();
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let server = ReleaseServer::new(store);
        let _watcher = server
            .watch()
            .map_err(|e| format!("watch {}: {e}", dir.display()))?;
        let fallback = server.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_FALLBACK);
            loop {
                tick.tick().await;
                let f = fallback.clone();
                let _ = tokio::task::spawn_blocking(move || f.refresh()).await;
            }
        });
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| format!("listen {listen}: {e}"))?;
        log::info!("serving {} on http://{listen}", dir.display());
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
