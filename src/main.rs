mod cli;
#[allow(dead_code)]
mod projection;
mod remote;
mod slurm_parser;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    let connection = remote::Connection::connect(&args.host).await?;
    let snapshot = connection.collect().await?;
    println!("Account: {}", snapshot.viewer);
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
