use anyhow::{bail, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Node {
    pub partitions: Vec<String>,
    pub state: String,
    pub allocated: u32,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub user: String,
    pub partition: String,
    pub state: String,
    pub nodes: u32,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
    pub node_list: String,
    pub time_limit: String,
    pub reason: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub nodes: Vec<Node>,
    pub jobs: Vec<Job>,
    pub collected: DateTime<Local>,
}

fn value<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .unwrap_or("")
}

fn number(value: &str) -> u32 {
    value.parse().unwrap_or(0)
}

fn timestamp(value: &str) -> Option<DateTime<Local>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|time| Utc.from_local_datetime(&time).single())
        .map(|time| time.with_timezone(&Local))
}

pub fn parse(nodes: &str, jobs: &str) -> Result<Snapshot> {
    let mut parsed_nodes = Vec::new();
    for line in nodes.lines().filter(|line| !line.trim().is_empty()) {
        let name = value(line, "NodeName");
        if name.is_empty() {
            bail!("unexpected scontrol node output: {line}");
        }
        parsed_nodes.push(Node {
            partitions: value(line, "Partitions")
                .split(',')
                .map(str::to_owned)
                .collect(),
            state: value(line, "State").into(),
            allocated: number(value(line, "CPUAlloc")),
        });
    }
    if parsed_nodes.is_empty() {
        bail!("Slurm returned no nodes");
    }

    let mut parsed_jobs = Vec::new();
    for line in jobs.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.splitn(11, '|').collect();
        if fields.len() != 11 {
            bail!("unexpected squeue output: {line}");
        }
        parsed_jobs.push(Job {
            id: fields[0].into(),
            user: fields[1].into(),
            partition: fields[2].into(),
            state: fields[3].into(),
            nodes: number(fields[4]),
            start: timestamp(fields[5]),
            end: timestamp(fields[6]),
            node_list: fields[7].into(),
            time_limit: fields[8].into(),
            name: fields[9].into(),
            reason: fields[10].into(),
        });
    }
    Ok(Snapshot {
        nodes: parsed_nodes,
        jobs: parsed_jobs,
        collected: Local::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_job_fields_and_utc_instants() {
        let snapshot = parse(
            "\nNodeName=n01 CPUAlloc=4 State=MIXED Partitions=batch,gpu\n",
            "\n42|alice|batch|RUNNING|2|2026-11-05T12:00:00|2026-11-05T13:00:00|n[01-02]|1:00:00|training|reason with spaces|detail\n",
        )
        .unwrap();
        assert_eq!(snapshot.nodes.len(), 1);
        let node = &snapshot.nodes[0];
        assert_eq!(node.partitions, ["batch", "gpu"]);
        assert_eq!(node.allocated, 4);
        assert_eq!(node.state, "MIXED");
        assert_eq!(snapshot.jobs.len(), 1);
        let job = &snapshot.jobs[0];
        assert_eq!(
            (&*job.id, &*job.user, &*job.partition),
            ("42", "alice", "batch")
        );
        assert_eq!((&*job.state, job.nodes), ("RUNNING", 2));
        assert_eq!((&*job.node_list, &*job.time_limit), ("n[01-02]", "1:00:00"));
        assert_eq!(job.name, "training");
        assert_eq!(job.reason, "reason with spaces|detail");
        assert_eq!(
            job.start.unwrap().with_timezone(&Utc),
            Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap()
        );
        assert_eq!(
            job.end.unwrap() - job.start.unwrap(),
            chrono::Duration::hours(1)
        );
    }

    #[test]
    fn rejects_incomplete_records_but_accepts_an_empty_queue() {
        let node = "NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p";
        assert!(parse(node, "\n").unwrap().jobs.is_empty());
        for nodes in ["", " \n", "State=IDLE Partitions=p", "NodeName= State=IDLE"] {
            assert!(parse(nodes, "").is_err(), "nodes: {nodes:?}");
        }
        assert!(parse(node, "42|alice|p|PENDING|1").is_err());
        let snapshot = parse(
            node,
            "42|alice|p|PENDING|1|N/A|Unknown|(null)|NOT_SET|job|Resources",
        )
        .unwrap();
        assert!(snapshot.jobs[0].start.is_none());
        assert!(snapshot.jobs[0].end.is_none());
    }
}
