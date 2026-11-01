mod airband;
mod audio;
mod config;
mod ingest;
mod input;
mod manifest;
mod pipeline;
mod spool;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::input::Airband;
use crate::pipeline::Pipeline;
use crate::spool::atomic_write;
use crate::spool::time::{iso_ms, now_ms};

const DEFAULT_CONFIG: &str = "/etc/read5/read5-uploader.toml";

#[derive(Parser)]
#[command(
    name = "read5-uploader",
    version,
    about = "Records rtl_airband's UDP audio streams and uploads them to Read5 as per-minute Opus archives"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record and upload until stopped (Ctrl-C or SIGTERM).
    Run {
        #[arg(long, short, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// List the rtl_airband udp_stream outputs `run` would record, and why
    /// any others are skipped.
    Streams {
        #[arg(long, short, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Check the config, list the streams, and ask Read5 whether the upload
    /// key is live. Changes nothing.
    Check {
        #[arg(long, short, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Streams { config } => {
            init_logging(None);
            let cfg = Config::load_offline(&config)?;
            print_streams(&cfg)
        }
        Cmd::Check { config } => {
            init_logging(None);
            let cfg = Config::load(&config)?;
            println!("config:   {} (ok)", config.display());
            println!("receiver: {}", cfg.receiver.id);
            println!("spool:    {}", cfg.spool.dir.display());
            print_streams(&cfg)?;
            if cfg.run.dry_run {
                println!("dry_run is on: nothing will be uploaded");
                return Ok(());
            }
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let store = ingest::Ingest::new(&cfg.upload.url, &cfg.upload.key);
            rt.block_on(store.check_key())?;
            println!("upload:   {} accepts the key", store.url());
            Ok(())
        }
        Cmd::Run { config } => {
            let cfg = Config::load(&config)?;
            let _guard = init_logging(cfg.run.log_dir.clone());
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(run(cfg))
        }
    }
}

fn print_streams(cfg: &Config) -> Result<()> {
    for s in input::resolve(cfg, &cfg.rtl_airband)? {
        println!("port {:5}  {} kHz  {:24}  {}", s.port, s.khz, s.channel, s.label);
    }
    Ok(())
}

fn init_logging(log_dir: Option<PathBuf>) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Colour only on a terminal: under systemd stderr goes to the journal.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let stderr = fmt::layer().with_writer(std::io::stderr).with_ansi(ansi).with_target(false);
    match log_dir {
        Some(dir) => {
            let _ = std::fs::create_dir_all(&dir);
            let appender = tracing_appender::rolling::daily(dir, "read5-uploader.log");
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let file = fmt::layer().with_writer(nb).with_ansi(false).with_target(false);
            tracing_subscriber::registry().with(filter).with(stderr).with(file).init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry().with(filter).with(stderr).init();
            None
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "read5-uploader starting");
    let scan = Duration::from_secs(2);
    let status_every = Duration::from_secs(cfg.run.status_secs.max(1));
    let status_file = cfg.run.status_file.clone();
    let dry = cfg.run.dry_run;

    let airband = Arc::new(Airband::start(&cfg)?);
    let mut p = Pipeline::new(cfg)?;
    {
        let a = airband.clone();
        p.set_down_source(Arc::new(move |mk: &str| a.down_channels(mk)));
    }
    if dry {
        warn!("dry_run: nothing will be uploaded or deleted");
    }
    info!(receiver = %p.receiver_id(), spool = %p.spool().root().display(), "recording continuously");
    systemd::ready();

    let mut last_status = std::time::Instant::now() - status_every;
    let mut stop = Box::pin(shutdown_signal());
    loop {
        tokio::select! {
            _ = &mut stop => {
                info!("shutdown requested");
                break;
            }
            _ = tokio::time::sleep(scan) => {}
        }
        step(&mut p, "scan", |p| p.scan_and_claim());
        step(&mut p, "seal", |p| p.seal_due());
        if airband.hold_uploads() {
            // Recording goes on; minutes wait in the spool until the clock is trusted.
        } else if let Err(e) = p.upload_due().await {
            error!(error = %format!("{e:#}"), "upload step failed");
        }
        step(&mut p, "delete", |p| p.delete_done());
        if last_status.elapsed() >= status_every {
            last_status = std::time::Instant::now();
            airband.prune_before(now_ms() - 3_600_000);
            write_status(&p, &airband, status_file.as_deref());
        }
        // A wedged receive thread stops the heartbeat, and systemd restarts us.
        if airband.threads_alive() {
            systemd::watchdog();
        }
    }
    systemd::stopping();

    // Clean shutdown: stop the UDP input (it writes its open utterances),
    // claim what is there, seal through now, try uploads for up to 30 s, mark
    // the run ended.
    airband.stop();
    step(&mut p, "final scan", |p| p.scan_and_claim());
    step(&mut p, "final seal", |p| p.seal_all_now());
    step(&mut p, "end run", |p| p.end_run());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Err(e) = p.upload_due().await {
            error!(error = %format!("{e:#}"), "upload step failed");
        }
        step(&mut p, "delete", |p| p.delete_done());
        let backlog = p.backlog_minutes().unwrap_or(u64::MAX);
        if (backlog == 0 && p.receiver_doc_uploaded()) || dry || std::time::Instant::now() >= deadline {
            if backlog > 0 && !dry {
                warn!(backlog_minutes = backlog, "exiting with pending uploads; they resume on next start");
            }
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        systemd::watchdog();
    }
    write_status(&p, &airband, status_file.as_deref());
    info!("stopped at {}", iso_ms(now_ms()));
    Ok(())
}

fn step(p: &mut Pipeline, what: &str, f: impl FnOnce(&mut Pipeline) -> Result<()>) {
    if let Err(e) = f(p) {
        p.stats.last_error = Some(format!("{what}: {e:#}"));
        error!(step = what, error = %format!("{e:#}"), "step failed");
    }
}

fn write_status(p: &Pipeline, airband: &Airband, path: Option<&std::path::Path>) {
    let Some(path) = path else { return };
    match p.status_json() {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.insert("rtlAirband".into(), airband.status_json());
                if airband.any_down() || airband.hold_uploads() {
                    o.insert("state".into(), "degraded".into());
                }
            }
            if let Err(e) = atomic_write(path, &serde_json::to_vec_pretty(&v).unwrap_or_default()) {
                warn!(error = %e, "could not write status file");
            }
        }
        Err(e) => warn!(error = %format!("{e:#}"), "could not build status"),
    }
}

/// Ctrl-C, or SIGTERM from systemd or Docker.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// systemd `Type=notify` readiness and watchdog. No-ops when not started by
/// systemd (no NOTIFY_SOCKET) and off Unix.
mod systemd {
    #[cfg(unix)]
    fn send(state: sd_notify::NotifyState) {
        let _ = sd_notify::notify(false, &[state]);
    }

    pub fn ready() {
        #[cfg(unix)]
        send(sd_notify::NotifyState::Ready);
    }

    pub fn watchdog() {
        #[cfg(unix)]
        send(sd_notify::NotifyState::Watchdog);
    }

    pub fn stopping() {
        #[cfg(unix)]
        send(sd_notify::NotifyState::Stopping);
    }
}
