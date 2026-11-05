use crate::slurm_parser::{self, Snapshot};
use anyhow::{bail, Context, Result};
use openssh::Session;
use std::{sync::Arc, time::Duration};
use tokio::process::Command;

const JOBS_SEPARATOR: &str = "\n__SQTOP_JOBS__\n";
const REMOTE_COMMAND: &str = "export TZ=UTC SLURM_TIME_FORMAT=standard; \
    scontrol -o show nodes && printf '\\n__SQTOP_JOBS__\\n' && \
    squeue -h -o '%i|%u|%P|%T|%D|%S|%e|%N|%l|%j|%R'";

#[derive(Clone)]
pub struct Connection {
    session: Arc<Session>,
    viewer: String,
}

impl Connection {
    pub async fn connect(host: &str) -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("sqtop-").tempdir()?;
        // openssh's connection builder forces BatchMode; authenticate before handing it the session.
        // -M: share the connection; -N: no remote command; -f: background after authentication.
        // -o ControlPersist=600: keep the master for 600 idle seconds; -S: control socket; --: end options.
        let status = Command::new("ssh")
            .args(["-M", "-N", "-f", "-o", "ControlPersist=600", "-S"])
            .arg(directory.path().join("master"))
            .args(["--", host])
            .kill_on_drop(true)
            .status()
            .await
            .context("starting SSH connection")?;
        if !status.success() {
            bail!("SSH connection failed ({status})");
        }
        let mut connection = Self {
            session: Arc::new(Session::new_process_mux(directory)),
            viewer: String::new(),
        };
        let output = connection
            .run("printf '%s\\n' \"${USER:-$(id -un)}\"")
            .await?;
        let viewer = String::from_utf8(output.stdout).context("remote account is not UTF-8")?;
        let viewer = viewer.trim();
        if viewer.is_empty() || viewer.chars().any(char::is_whitespace) {
            bail!("invalid remote account name");
        }
        connection.viewer = viewer.to_owned();
        Ok(connection)
    }

    async fn run(&self, script: &str) -> Result<std::process::Output> {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            self.session.raw_command(script).output(),
        )
        .await
        .context("SSH command timed out")?
        .context("running remote command")?;
        if !output.status.success() {
            bail!(
                "SSH/Slurm command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    pub async fn collect(&self) -> Result<Snapshot> {
        let output = self.run(REMOTE_COMMAND).await?;
        let text = String::from_utf8(output.stdout).context("Slurm output is not UTF-8")?;
        let mut snapshot = decode(&text)?;
        snapshot.viewer = self.viewer.clone();
        Ok(snapshot)
    }
}

fn decode(text: &str) -> Result<Snapshot> {
    let (nodes, jobs) = text
        .split_once(JOBS_SEPARATOR)
        .context("incomplete Slurm output")?;
    slurm_parser::parse(nodes, jobs)
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
