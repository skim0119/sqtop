use anyhow::{bail, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct Node {
    pub partitions: Vec<String>,
    pub state: String,
    pub allocated: u32,
}

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

#[derive(Clone, Debug)]
pub struct CpuNode {
    pub name: String,
    pub allocated: u32,
    pub idle: u32,
    pub other: u32,
    pub total: u32,
    pub load: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub nodes: Vec<Node>,
    pub jobs: Vec<Job>,
    pub collected: DateTime<Local>,
    pub viewer: String,
    pub cpu: Option<Vec<CpuNode>>,
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
        viewer: String::new(),
        cpu: None,
    })
}

pub fn parse_cpu(text: &str) -> Result<Vec<CpuNode>> {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        if fields.len() != 3 || fields[0].is_empty() {
            bail!("unexpected sinfo output: {line}");
        }
        let name = fields[0];
        let values: Vec<u32> = fields[1]
            .split('/')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| anyhow::anyhow!("unexpected sinfo CPU counts: {line}"))?;
        if values.len() != 4 {
            bail!("unexpected sinfo output: {line}");
        }
        let load = fields[2]
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0);
        if seen.insert(name.to_owned()) {
            nodes.push(CpuNode {
                name: name.into(),
                allocated: values[0],
                idle: values[1],
                other: values[2],
                total: values[3],
                load,
            });
        }
    }
    Ok(nodes)
}

// shortcut: expansion stops at 100,000 hosts; use Slurm hostlist support if larger lists are needed.
pub(crate) fn hosts(list: &str) -> Option<HashSet<String>> {
    const LIMIT: usize = 100_000;
    if list.is_empty() || list == "(null)" || list.contains(char::is_whitespace) {
        return None;
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_brackets = false;
    for (index, character) in list.char_indices() {
        match character {
            '[' if !in_brackets => in_brackets = true,
            '[' => return None,
            ']' if in_brackets => in_brackets = false,
            ']' => return None,
            ',' if !in_brackets => {
                parts.push(&list[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if in_brackets {
        return None;
    }
    parts.push(&list[start..]);

    let mut result = HashSet::new();
    for part in parts {
        if part.is_empty() {
            return None;
        }
        let mut names = vec![String::new()];
        let mut rest = part;
        while let Some(open) = rest.find('[') {
            let close = rest.find(']')?;
            if close < open {
                return None;
            }
            let prefix = &rest[..open];
            let mut numbers = Vec::new();
            for range in rest[open + 1..close].split(',') {
                if let Some((first, last)) = range.split_once('-') {
                    let first_number: u32 = first.parse().ok()?;
                    let last_number: u32 = last.parse().ok()?;
                    if last_number < first_number
                        || u64::from(last_number) - u64::from(first_number) >= LIMIT as u64
                    {
                        return None;
                    }
                    for number in first_number..=last_number {
                        numbers.push(format!("{number:0width$}", width = first.len()));
                        if numbers.len() > LIMIT {
                            return None;
                        }
                    }
                } else {
                    if range.is_empty() || !range.bytes().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    numbers.push(range.to_owned());
                    if numbers.len() > LIMIT {
                        return None;
                    }
                }
            }
            if names
                .len()
                .checked_mul(numbers.len())?
                .saturating_add(result.len())
                > LIMIT
            {
                return None;
            }
            names = names
                .iter()
                .flat_map(|name| {
                    numbers
                        .iter()
                        .map(move |number| format!("{name}{prefix}{number}"))
                })
                .collect();
            rest = &rest[close + 1..];
        }
        for mut name in names {
            name.push_str(rest);
            result.insert(name);
            if result.len() > LIMIT {
                return None;
            }
        }
    }
    Some(result)
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
        assert!(snapshot.viewer.is_empty());
        assert!(snapshot.cpu.is_none());
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

    #[test]
    fn cpu_load_is_optional_but_counts_must_be_valid() {
        for load in ["N/A", "NaN", "inf", "-inf", "-0.1", ""] {
            let nodes = parse_cpu(&format!(" n1 | 2/3/1/6 | {load}\n")).unwrap();
            assert_eq!(nodes[0].load, None, "load: {load:?}");
            assert_eq!(nodes[0].other, 1);
        }
        assert_eq!(parse_cpu("n1|0/0/0/0|0").unwrap()[0].load, Some(0.0));
        assert!(parse_cpu("\n ").unwrap().is_empty());
        for record in [
            "|0/0/0/0|0",
            "n1|1/2/3|0",
            "n1|1/2/3/4/5|0",
            "n1|-1/0/0/0|0",
            "n1|4294967296/0/0/0|0",
            "n1|a/0/0/0|0",
            "n1|0/0/0/0",
            "n1|0/0/0/0|0|extra",
        ] {
            assert!(parse_cpu(record).is_err(), "record: {record:?}");
        }
    }

    #[test]
    fn hostlist_expansion_preserves_padding_products_and_uniqueness() {
        let expanded = hosts("rack[1-2]n[01-02],rack1n01,standalone").unwrap();
        let expected = ["rack1n01", "rack1n02", "rack2n01", "rack2n02", "standalone"]
            .into_iter()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        assert_eq!(expanded, expected);
        for list in [
            "",
            "(null)",
            "n 1",
            ",n1",
            "n1,",
            "n[]",
            "n[2-1]",
            "n[a]",
            "n[[1]]",
            "n1]",
            "n[1",
            "n[1-100001]",
            "n[1-400]x[1-400]",
        ] {
            assert!(hosts(list).is_none(), "hostlist: {list:?}");
        }
    }

    #[test]
    fn parses_sinfo_counts_and_load_without_double_counting_nodes() {
        let nodes = parse_cpu("n1|4/4/0/8|1.25\nn1|4/4/0/8|1.25\nn2|0/8/0/8|N/A\n").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(
            (nodes[0].allocated, nodes[0].idle, nodes[0].total),
            (4, 4, 8)
        );
        assert_eq!(nodes[0].load, Some(1.25));
        assert_eq!(nodes[1].load, None);
        assert!(parse_cpu("n1|invalid|1.0\n").is_err());
    }

    #[test]
    fn hostlist_ranges() {
        let hosts = hosts("n[01-03,05],x7").unwrap();
        assert!(hosts.contains("n02"));
        assert_eq!(hosts.len(), 5);
        let multi = super::hosts("n[1-2]x[01-02]").unwrap();
        assert_eq!(multi.len(), 4);
        assert!(multi.contains("n2x01"));
        assert!(super::hosts("n[1-2]x[1-100000]").is_none());
        assert!(super::hosts("n[1-2").is_none());
    }
}
