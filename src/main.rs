//! Run the terminal dashboard: collect Slurm snapshots over SSH, compute projections,
//! and handle keyboard input. Background tasks send results back to this loop;
//! only the loop updates application state and draws the screen.

mod cli;
mod projection;
mod remote;
mod slurm_parser;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const REFRESH_SECONDS: u64 = 300;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    // Authenticate before raw mode so SSH can use the terminal for login prompts.
    let (connection, received) = remote::Connection::connect(&args.host).await?;
    // Separate channels carry network results and CPU-bound projection results.
    let (tx, mut rx) = mpsc::channel(1);
    let (estimate_tx, mut estimate_rx) = mpsc::unbounded_channel();
    let mut generation = 0_u64;
    let mut app = ui::App::new(args.host);
    app.traffic.requests = 1; // Remote account lookup.
    app.traffic.received = received;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // Raw mode delivers individual key presses; the alternate screen preserves shell contents.
    enable_raw_mode()?;
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
        disable_raw_mode()?;
        return Err(error.into());
    }
    // Capture loop errors so ordinary failures still reach terminal cleanup below.
    let result = async {
        terminal.clear()?;
        let mut next_refresh = Instant::now(); // Fetch immediately on startup.
        loop {
            // Keep at most one SSH collection in flight, including manual refreshes.
            if Instant::now() >= next_refresh && !app.loading {
                app.loading = true;
                app.traffic.requests += 1;
                let tx = tx.clone();
                let connection = connection.clone();
                let include_cpu = app.tab == 1; // Only the Jobs view needs sinfo node loads.
                tokio::spawn(async move {
                    let mut received = 0;
                    let result = connection.collect(include_cpu, &mut received).await;
                    let _ = tx.send((result, received)).await;
                });
                next_refresh = Instant::now() + Duration::from_secs(REFRESH_SECONDS);
            }
            // Never wait on a worker here; drawing and keyboard handling must keep running.
            if let Ok((result, received)) = rx.try_recv() {
                app.loading = false;
                app.traffic.received += received;
                match result {
                    Ok(snapshot) => {
                        app.set_snapshot(snapshot);
                    }
                    Err(error) => app.error = Some(format!("{error:#}")),
                }
            }
            if app.estimate_dirty {
                generation += 1;
                app.estimate_dirty = false;
                app.projection = None;
                if let Some((snapshot, partition)) = app.estimate_input() {
                    let tx = estimate_tx.clone();
                    let current = generation;
                    // Scheduling is CPU work; keep it off the async executor's worker threads.
                    tokio::task::spawn_blocking(move || {
                        let now = chrono::Local::now();
                        let projection = projection::Projection::queue(&snapshot, &partition, now);
                        let _ = tx.send((current, projection));
                    });
                }
            }
            while let Ok((id, projection)) = estimate_rx.try_recv() {
                // Ignore calculations started before the latest snapshot or selection change.
                if id == generation {
                    app.projection = Some(projection);
                }
            }
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // While editing an account, Esc and typed characters belong to the editor.
                    if app.editing_user.is_some() {
                        app.key(key.code);
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        // Entering Jobs requests fresh node loads instead of waiting for the next poll.
                        KeyCode::Char('2') => {
                            app.key(key.code);
                            next_refresh = Instant::now();
                        }
                        KeyCode::Char('r') => next_refresh = Instant::now(),
                        _ => app.key(key.code),
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // Restore the shell before returning any error captured from the loop.
    disable_raw_mode().context("restoring terminal")?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
