use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use tokio::signal::unix::{signal, SignalKind};
use photo_clearout::config::{self, Config};
use photo_clearout::tunnel;
use photo_clearout::immich::{ImmichClient, PhotoSource};
use photo_clearout::web::{self, AppState, Recovery};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// True when this process is in the foreground of a terminal window: started
/// normally from a terminal, not with `&`, `nohup`, as a service, or moved
/// to the background with Ctrl+Z + `bg`.
fn in_foreground() -> bool {
    // SAFETY: plain queries about this process and its terminal; they read
    // no memory of ours and can't fail in a way that matters here.
    unsafe { libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp() }
}

/// Completes if the tool is ever moved to the background.
async fn moved_to_background() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if !in_foreground() {
            return;
        }
    }
}

const HELP: &str = "\
photo-clearout: clear out old photos in Immich, oldest first, in a grid.

Start it with ./run.sh in this folder. It reads Immich's address and where
the API key is in 1Password from .env, then opens the page in your browser.
It stops when you press Finish, close the tab, close the terminal or press
Ctrl+C. Photos only ever go to Immich's trash (restorable for 30 days).

Settings (environment variables):
  IMMICH_URL                 Immich's Tailscale address, e.g. http://100.x.x.x:2283
  IMMICH_API_KEY             the key (run.sh fetches it from 1Password)
  PHOTO_CLEAROUT_PORT        local port for the page (default 3000)
  PHOTO_CLEAROUT_STATE_DIR   where marks and progress are saved (default .state)
  PHOTO_CLEAROUT_NO_BROWSER  set to 1 to not open the browser

Options:
  -h, --help     show this help
  -V, --version  show the version

Everything else is in README.md.
";

async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {}
        ["-h" | "--help"] => {
            print!("{HELP}");
            return Ok(());
        }
        ["-V" | "--version"] => {
            println!("photo-clearout {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        other => anyhow::bail!("unknown option {:?}; see --help", other.join(" ")),
    }

    // This tool must never run in the background: only while you use it.
    if !in_foreground() {
        anyhow::bail!(
            "photo-clearout only runs in the foreground of a terminal window. \
             Start it with ./run.sh in a terminal (not with &, nohup or as a service)."
        );
    }

    let config = Config::from_env(|k| std::env::var(k).ok())?;
    let url = reqwest::Url::parse(&config.immich_url).context("IMMICH_URL is not a valid address")?;
    let client = if config::needs_tunnel(&url) {
        // Plain http:// is only safe if it really goes through Tailscale.
        let tunnel = tunnel::through_tailscale(&url)?;
        println!(
            "Immich is reached through Tailscale ({} → {}, interface {}).",
            tunnel.local,
            tunnel.remote.ip(),
            tunnel.interface
        );
        ImmichClient::through_tunnel(&config.immich_url, &config.api_key, &tunnel)?
    } else {
        ImmichClient::new(&config.immich_url, &config.api_key)?
    };

    // Fail now, with a clear message, rather than halfway through a session.
    println!("Checking Immich at {} …", config.immich_url);
    client
        .assets_taken_after(None, 1)
        .await
        .context("could not use Immich")?;
    // Also check the key may trash and restore, so a missing permission
    // shows up now and not halfway through a session. Nothing is changed.
    client
        .check_can_trash_and_restore()
        .await
        .context("the API key can't do everything the tool needs")?;
    println!("Immich OK.");

    // Everything the tool saves goes in one folder inside the project.
    println!("Progress, settings, marks and undo list are saved in {}/", config.state_dir.display());
    // Only you can read it: it holds file names and places.
    photo_clearout::progress::prepare_state_dir(&config.state_dir)
        .with_context(|| format!("could not set up {}", config.state_dir.display()))?;
    let state = AppState::new(Arc::new(client), &config.state_dir, web::random_token(), config.port);

    // If the tool was stopped in the middle of trashing last time, ask
    // Immich how far it got and fix the marks and undo list to match.
    let recovery = state.recover().await.context("could not check an unfinished trash from last time")?;
    if recovery != Recovery::default() {
        println!(
            "Repaired an unfinished trash from last time: {} were trashed (undo list kept), {} were not (still marked).",
            recovery.completed, recovery.not_done
        );
        if recovery.unchecked > 0 {
            println!("{} could not be checked with Immich; they'll be checked again next time.", recovery.unchecked);
        }
    }

    // 127.0.0.1 only: nothing else on the network can connect.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .with_context(|| {
            format!(
                "could not listen on 127.0.0.1:{} (is the tool already running? set PHOTO_CLEAROUT_PORT to use another port)",
                config.port
            )
        })?;

    let url = format!("http://127.0.0.1:{}/", config.port);
    println!(
        "\nReady: {url}\nIt stops when you press Finish, close the browser tab, close this terminal, or press Ctrl+C.\nMarks and the undo list are always kept."
    );
    if config.open_browser {
        // Best effort; if it fails, the URL above is still printed.
        // The browser never needs the API key, so it isn't passed on.
        let _ = std::process::Command::new("open").arg(&url).env_remove("IMMICH_API_KEY").spawn();
    }

    // Every way the tool stops. Graceful shutdown lets the page receive its
    // last reply first. Signals are set up before serving so none is missed.
    let mut hangup = signal(SignalKind::hangup()).context("could not watch for the terminal closing")?;
    let mut terminate = signal(SignalKind::terminate()).context("could not watch for stop requests")?;
    let watcher = state.clone();
    axum::serve(listener, web::router(state.clone()))
        .with_graceful_shutdown(async move {
            let why = tokio::select! {
                () = watcher.finished() => "Finished",
                () = watcher.page_closed(web::GOODBYE_GRACE) => "The browser tab was closed",
                () = watcher.idle_for(web::IDLE_LIMIT) => "No browser page has been in touch for 2 minutes",
                _ = tokio::signal::ctrl_c() => "Ctrl+C",
                _ = hangup.recv() => "The terminal was closed",
                _ = terminate.recv() => "Asked to stop",
                () = moved_to_background() => "Moved to the background, which isn't allowed",
            };
            println!("\n{why}. Stopped. Marks and the undo list are kept for next time.");
        })
        .await?;

    // Never exit in the middle of trashing or restoring.
    state.jobs_done().await;
    Ok(())
}
