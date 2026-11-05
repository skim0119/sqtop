use crate::model::{self, Snapshot};
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::process::Command;

const JOBS_SEPARATOR: &str = "\n__SQTOP_JOBS__\n";
const REMOTE_COMMAND: &str = "export TZ=UTC SLURM_TIME_FORMAT=standard; \
    scontrol -o show nodes && printf '\\n__SQTOP_JOBS__\\n' && \
    squeue -h -o '%i|%u|%P|%T|%D|%S|%e|%N|%l|%j|%R'";

pub async fn collect(host: &str) -> Result<Snapshot> {
    let mut command = Command::new("ssh");
    command
        .args(["--", host, REMOTE_COMMAND])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("SSH command timed out")?
        .context("starting ssh")?;
    if !output.status.success() {
        bail!(
            "SSH/Slurm command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout).context("Slurm output is not UTF-8")?;
    decode(&text)
}

fn decode(text: &str) -> Result<Snapshot> {
    let (nodes, jobs) = text
        .split_once(JOBS_SEPARATOR)
        .context("incomplete Slurm output")?;
    model::parse(nodes, jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_complete_output_but_accepts_an_empty_queue() {
        let node = "NodeName=n1 CPUAlloc=0 State=IDLE Partitions=batch";
        assert!(decode(node).is_err());
        let snapshot = decode(&format!("{node}{JOBS_SEPARATOR}")).unwrap();
        assert_eq!(snapshot.nodes.len(), 1);
        assert!(snapshot.jobs.is_empty());
    }
}
