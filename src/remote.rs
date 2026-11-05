use crate::slurm_parser::{self, Snapshot};
use anyhow::{bail, Context, Result};
use openssh::Session;
use std::{sync::Arc, time::Duration};
use tokio::process::Command;

const CPU_SEPARATOR: &str = "\n__SQTOP_CPU__\n";
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
        let session = match existing_session(host).await? {
            Some(session) => session,
            None => new_session(host).await?,
        };
        let mut connection = Self {
            session: Arc::new(session),
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

    pub async fn collect(&self, include_cpu: bool) -> Result<Snapshot> {
        let remote_command = if include_cpu {
            format!("{REMOTE_COMMAND} && printf '\\n__SQTOP_CPU__\\n' && sinfo -N -h -o '%N|%C|%O'")
        } else {
            REMOTE_COMMAND.to_owned()
        };
        let output = self.run(&remote_command).await?;
        let text = String::from_utf8(output.stdout).context("Slurm output is not UTF-8")?;
        let (jobs, cpu) = if include_cpu {
            let (jobs, cpu) = text
                .split_once(CPU_SEPARATOR)
                .context("incomplete sinfo output")?;
            (jobs, Some(slurm_parser::parse_cpu(cpu)?))
        } else {
            (&*text, None)
        };
        let mut snapshot = decode(jobs)?;
        snapshot.cpu = cpu;
        snapshot.viewer = self.viewer.clone();
        Ok(snapshot)
    }
}

// ssh -G expands aliases, tilde, and ControlPath tokens using OpenSSH's own rules.
async fn existing_session(host: &str) -> Result<Option<Session>> {
    let mut command = Command::new("ssh");
    command.args(["-G", "--", host]).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("reading SSH configuration timed out")?
        .context("reading SSH configuration")?;
    if !output.status.success() {
        bail!(
            "reading SSH configuration failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let config = String::from_utf8(output.stdout).context("SSH configuration is not UTF-8")?;
    let Some(path) = control_path(&config) else {
        return Ok(None);
    };
    // A resumed session borrows the master: dropping it must not close the user's connection.
    let session = Session::resume(std::path::PathBuf::from(path).into_boxed_path(), None);
    match tokio::time::timeout(Duration::from_secs(20), session.check()).await {
        Ok(Ok(())) => Ok(Some(session)),
        Ok(Err(_)) => Ok(None),
        Err(error) => Err(error).context("checking existing SSH connection timed out"),
    }
}

fn control_path(config: &str) -> Option<&str> {
    config
        .lines()
        .find_map(|line| line.strip_prefix("controlpath "))
        .filter(|path| !path.is_empty() && *path != "none")
}

async fn new_session(host: &str) -> Result<Session> {
    let directory = control_directory()?;
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
    Ok(Session::new_process_mux(directory))
}

fn control_directory() -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    // macOS TMPDIR paths can exceed the Unix socket limit, including ssh's temporary suffix.
    tempfile::Builder::new()
        .prefix("sqtop-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
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

    #[tokio::test]
    #[ignore = "requires SQTOP_TEST_SSH_HOST with an existing authenticated SSH master"]
    async fn reuses_master_without_closing_it() {
        let host = std::env::var("SQTOP_TEST_SSH_HOST").unwrap();
        let session = existing_session(&host)
            .await
            .unwrap()
            .expect("existing SSH master");
        session.check().await.unwrap();
        drop(session);
        assert!(existing_session(&host).await.unwrap().is_some());
    }

    #[test]
    fn reads_configured_control_paths() {
        assert_eq!(
            control_path("hostname cluster\ncontrolpath /tmp/socket\n"),
            Some("/tmp/socket")
        );
        assert_eq!(
            control_path("controlpath /tmp/path with spaces/socket\n"),
            Some("/tmp/path with spaces/socket")
        );
        for config in [
            "",
            "controlpath none\n",
            "controlpath \n",
            "hostname cluster\n",
        ] {
            assert_eq!(control_path(config), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_fits_and_stays_private() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        let directory = control_directory().unwrap();
        let socket = directory.path().join("master");
        // OpenSSH may append a temporary suffix while creating the socket.
        assert!(socket.as_os_str().len() + 17 < 104);
        assert_eq!(
            directory.path().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _listener = UnixListener::bind(&socket).unwrap();
    }

    #[test]
    fn requires_complete_output_but_accepts_an_empty_queue() {
        let node = "NodeName=n1 CPUAlloc=0 State=IDLE Partitions=batch";
        assert!(decode(node).is_err());
        let snapshot = decode(&format!("{node}{JOBS_SEPARATOR}")).unwrap();
        assert_eq!(snapshot.nodes.len(), 1);
        assert!(snapshot.jobs.is_empty());
    }
}
