use crate::slurm_parser::{self, Snapshot};
use anyhow::{bail, Context, Result};
use openssh::Session;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::process::Command;

const CPU_SEPARATOR: &str = "\n__SQTOP_CPU__\n";
const JOBS_SEPARATOR: &str = "\n__SQTOP_JOBS__\n";
const REMOTE_COMMAND: &str = "export TZ=UTC SLURM_TIME_FORMAT=standard; \
    scontrol -o show nodes && printf '\\n__SQTOP_JOBS__\\n' && \
    squeue -h -o '%i|%u|%P|%T|%D|%S|%e|%N|%l|%j|%R'";

// One short-lived, read-only probe per node; no agent or controller query.
const NODE_PROBE: &str = r#"awk '/^MemTotal:/ {t=$2} /^MemAvailable:/ {a=$2} END {if(t && a != "") printf "RAM|%s|%s\n",t,a}' /proc/meminfo
nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | sed 's/^/GPU|/'"#;

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn usage_targets(snapshot: &Snapshot, viewer: &str) -> Vec<String> {
    let mut targets = std::collections::BTreeSet::new();
    for job in &snapshot.jobs {
        if job.user == viewer && job.state == "RUNNING" {
            if let Some(hosts) = slurm_parser::hosts(&job.node_list) {
                targets.extend(hosts.into_iter().filter(|host| {
                    !host.starts_with('-')
                        && host
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
                }));
            }
        }
    }
    targets.into_iter().collect()
}

fn rotate_targets(targets: &mut [String], offset: &AtomicUsize) {
    if !targets.is_empty() {
        let start = offset.fetch_add(4, Ordering::Relaxed) % targets.len();
        targets.rotate_left(start);
    }
}

fn usage_command(targets: &[String]) -> String {
    let worker = format!(
        r#"host=$1
output=$(timeout 6 ssh -n -T -o BatchMode=yes -o ConnectTimeout=3 -o ConnectionAttempts=1 -o StrictHostKeyChecking=accept-new -- "$host" {} 2>&1)
status=$?
printf '%s\n' "$output" | sed "s/^/$host|/"
if [ "$status" -ne 0 ]; then
    case "$status:$output" in
        124:*) reason="node probe timed out" ;;
        *"Host key verification failed"*|*"REMOTE HOST IDENTIFICATION HAS CHANGED"*) reason="SSH host key verification failed" ;;
        *"Permission denied"*) reason="SSH access denied (job may have ended)" ;;
        *) reason="SSH connection or node command failed (exit $status)" ;;
    esac
    printf '%s|ERROR|%s\n' "$host" "$reason"
fi
"#,
        shell_quote(NODE_PROBE)
    );
    // The whole batch also has a deadline: large allocations cannot delay refresh
    // indefinitely. Completed probes are retained; remaining nodes show unavailable.
    format!(
        "printf '%s\n' {} | timeout 15 xargs -n 1 -P 4 sh -c {} sh; true",
        targets
            .iter()
            .map(|host| shell_quote(host))
            .collect::<Vec<_>>()
            .join(" "),
        shell_quote(&worker)
    )
}

#[derive(Clone)]
pub struct Connection {
    session: Arc<Session>,
    viewer: String,
    probe_offset: Arc<AtomicUsize>,
}

impl Connection {
    pub async fn connect(host: &str) -> Result<(Self, u64)> {
        let session = match existing_session(host).await? {
            Some(session) => session,
            None => new_session(host).await?,
        };
        let mut connection = Self {
            session: Arc::new(session),
            viewer: String::new(),
            probe_offset: Arc::new(AtomicUsize::new(0)),
        };
        let mut received = 0;
        let output = connection
            .run("printf '%s\\n' \"${USER:-$(id -un)}\"", &mut received)
            .await?;
        let viewer = String::from_utf8(output.stdout).context("remote account is not UTF-8")?;
        let viewer = viewer.trim();
        if viewer.is_empty() || viewer.chars().any(char::is_whitespace) {
            bail!("invalid remote account name");
        }
        connection.viewer = viewer.to_owned();
        Ok((connection, received))
    }

    async fn run(&self, script: &str, received: &mut u64) -> Result<std::process::Output> {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            self.session.raw_command(script).output(),
        )
        .await
        .context("SSH command timed out")?
        .context("running remote command")?;
        // Count completed output even when the command exits with an error.
        *received += (output.stdout.len() + output.stderr.len()) as u64;
        if !output.status.success() {
            bail!(
                "SSH/Slurm command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    pub async fn collect(
        &self,
        include_cpu: bool,
        received: &mut u64,
        requests: &mut u64,
    ) -> Result<Snapshot> {
        let remote_command = if include_cpu {
            format!("{REMOTE_COMMAND} && printf '\\n__SQTOP_CPU__\\n' && sinfo -N -h -o '%N|%C|%O'")
        } else {
            REMOTE_COMMAND.to_owned()
        };
        *requests += 1;
        let output = self.run(&remote_command, received).await?;
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
        if include_cpu {
            let mut targets = usage_targets(&snapshot, &self.viewer);
            for node in &mut snapshot.nodes {
                node.probe_requested = targets.binary_search(&node.name).is_ok();
            }
            if !targets.is_empty() {
                // Start with a different worker group so the batch deadline cannot
                // permanently starve nodes at the end of a large allocation.
                rotate_targets(&mut targets, &self.probe_offset);
                *requests += 1;
                if let Ok(output) = self.run(&usage_command(&targets), received).await {
                    slurm_parser::apply_usage(
                        &mut snapshot,
                        &String::from_utf8_lossy(&output.stdout),
                    );
                }
            }
        }
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
    #[test]
    fn rotating_worker_groups_eventually_visit_every_node() {
        let offset = AtomicUsize::new(0);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..5 {
            let mut targets: Vec<_> = (0..19).map(|i| format!("n{i:02}")).collect();
            rotate_targets(&mut targets, &offset);
            seen.extend(targets.into_iter().take(4));
        }
        assert_eq!(seen.len(), 19);
    }

    #[cfg(unix)]
    #[test]
    fn shell_batch_preserves_partial_metrics_and_classifies_errors() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        // Run the actual generated batch, replacing only external timeout/SSH.
        let timeout = directory.path().join("timeout");
        std::fs::write(&timeout, "#!/bin/sh\nif [ \"$1\" = 15 ]; then shift; exec \"$@\"; fi\ncase \"$*\" in *n1*) printf 'RAM|8388608|2097152\\n'; exit 124;; *) echo 'Permission denied' >&2; exit 255;; esac\n").unwrap();
        std::fs::set_permissions(&timeout, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            directory.path().display(),
            std::env::var("PATH").unwrap()
        );
        let output = std::process::Command::new("sh")
            .args(["-c", &usage_command(&["n1".into(), "n2".into()])])
            .env("PATH", path)
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("n1|RAM|8388608|2097152"), "{text}");
        assert!(text.contains("n1|ERROR|node probe timed out"), "{text}");
        assert!(text.contains("n2|ERROR|SSH access denied"), "{text}");
    }

    #[test]
    fn probe_targets_only_authenticated_users_running_nodes() {
        let snapshot = slurm_parser::parse("NodeName=n1 CPUAlloc=1 State=ALLOCATED Partitions=p", "1|me|p|RUNNING|2|N/A|N/A|n[1-2]|1:00:00|a|n1\n2|other|p|RUNNING|1|N/A|N/A|n3|1:00:00|b|n3\n3|me|p|PENDING|1|N/A|N/A|n4|1:00:00|c|Priority").unwrap();
        assert_eq!(usage_targets(&snapshot, "me"), vec!["n1", "n2"]);
        assert!(usage_command(&usage_targets(&snapshot, "me")).contains("-P 4"));
        assert!(usage_command(&usage_targets(&snapshot, "me"))
            .contains("StrictHostKeyChecking=accept-new"));
    }

    #[tokio::test]
    #[ignore = "requires authenticated SQTOP_TEST_SSH_HOST and a running GPU job"]
    async fn live_node_usage() {
        let host = std::env::var("SQTOP_TEST_SSH_HOST").unwrap();
        let (connection, _) = Connection::connect(&host).await.unwrap();
        let snapshot = connection.collect(true, &mut 0, &mut 0).await.unwrap();
        let measured: Vec<_> = snapshot
            .nodes
            .iter()
            .filter_map(|node| node.usage.as_ref())
            .collect();
        assert!(!measured.is_empty(), "no node telemetry received");
        assert!(measured.iter().all(|usage| usage.ram.is_some()));
        assert!(measured.iter().any(|usage| !usage.gpus.is_empty()));
        eprintln!(
            "Measured RAM on {} nodes; GPU on {} nodes",
            measured.len(),
            measured
                .iter()
                .filter(|usage| !usage.gpus.is_empty())
                .count()
        );
    }

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
