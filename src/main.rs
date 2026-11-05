mod cli;
mod model;
mod remote;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    let snapshot = remote::collect(&args.host).await?;
    println!(
        "{} | {} nodes | {} jobs",
        snapshot.collected,
        snapshot.nodes.len(),
        snapshot.jobs.len()
    );
    for job in snapshot.jobs {
        println!(
            "{} | {} | {} | {} | {} nodes | {}",
            job.id, job.user, job.partition, job.state, job.nodes, job.name
        );
    }
    Ok(())
}
