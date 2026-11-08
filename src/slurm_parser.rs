use anyhow::{bail, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct NodeUsage {
    pub ram: Option<(u64, u64)>, // Total and available KiB, including reclaimable cache.
    pub gpus: Vec<GpuUsage>,
}

#[derive(Clone, Debug)]
pub struct GpuUsage {
    pub index: u32,
    pub name: String,
    pub utilization: Option<f64>,
    pub used_mib: Option<u64>,
    pub total_mib: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GpuAllocation {
    pub model: String,
    pub total: u32,
    pub allocated: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub probe_requested: bool,
    pub probe_error: Option<String>,
    pub usage: Option<NodeUsage>,
    pub cpus: Option<u32>,
    pub gpus: Vec<GpuAllocation>,
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

// Prefer typed TRES entries; the untyped total describes the same devices.
fn gpu_counts(text: &str, prefix: &str) -> Vec<(String, u32)> {
    text.split(',')
        .filter_map(|entry| {
            let (kind, count) = entry.split_once('=')?;
            let model = kind.strip_prefix(prefix)?;
            if !model.is_empty() && !model.starts_with(':') {
                return None;
            }
            Some((
                model.trim_start_matches(':').to_owned(),
                count.parse().ok()?,
            ))
        })
        .collect()
}

fn gres_counts(text: &str) -> Vec<(String, u32)> {
    // Index/socket annotations can contain commas; remove them before splitting.
    let mut depth = 0;
    let plain: String = text
        .chars()
        .filter(|ch| match ch {
            '(' => {
                depth += 1;
                false
            }
            ')' => {
                depth -= 1;
                false
            }
            _ => depth == 0,
        })
        .collect();
    plain
        .split(',')
        .filter_map(|entry| {
            let entry = entry.strip_prefix("gpu:")?;
            let (model, count) = entry.rsplit_once(':').unwrap_or(("", entry));
            Some((model.to_owned(), count.parse().ok()?))
        })
        .collect()
}

fn node_gpus(line: &str) -> Vec<GpuAllocation> {
    let mut configured = gpu_counts(value(line, "CfgTRES"), "gres/gpu");
    if configured.is_empty() {
        configured = gres_counts(value(line, "Gres"));
    }
    if configured.iter().any(|(model, _)| !model.is_empty()) {
        configured.retain(|(model, _)| !model.is_empty());
    }
    let allocated_text = value(line, "AllocTRES");
    let allocated = gpu_counts(allocated_text, "gres/gpu");
    let fallback = gres_counts(value(line, "GresUsed"));
    let single = configured.len() == 1;
    configured
        .into_iter()
        .map(|(model, total)| {
            let used = allocated
                .iter()
                .find(|(kind, _)| kind == &model)
                .or_else(|| {
                    if single {
                        allocated.iter().find(|(kind, _)| kind.is_empty())
                    } else {
                        None
                    }
                })
                .map(|(_, count)| *count)
                .or_else(|| {
                    fallback
                        .iter()
                        .find(|(kind, _)| kind == &model)
                        .or_else(|| {
                            if single {
                                fallback.iter().find(|(kind, _)| kind.is_empty())
                            } else {
                                None
                            }
                        })
                        .map(|(_, count)| *count)
                })
                .or_else(|| {
                    if !allocated_text.is_empty()
                        && allocated_text != "(null)"
                        && allocated.is_empty()
                    {
                        Some(0)
                    } else {
                        None
                    }
                });
            GpuAllocation {
                model,
                total,
                allocated: used,
            }
        })
        .collect()
}

pub fn apply_usage(snapshot: &mut Snapshot, text: &str) {
    for line in text.lines() {
        let fields: Vec<_> = line.splitn(3, '|').collect();
        if fields.len() != 3 {
            continue;
        }
        let Some(node) = snapshot
            .nodes
            .iter_mut()
            .find(|node| node.name == fields[0] && node.probe_requested)
        else {
            continue;
        };
        match fields[1] {
            "ERROR" => node.probe_error = Some(fields[2].chars().take(160).collect()),
            "RAM" => {
                let Some((total, available)) = fields[2].split_once('|') else {
                    continue;
                };
                if let (Ok(total), Ok(available)) = (total.parse::<u64>(), available.parse::<u64>())
                {
                    if total > 0 && available <= total {
                        node.usage
                            .get_or_insert_with(|| NodeUsage {
                                ram: None,
                                gpus: Vec::new(),
                            })
                            .ram = Some((total, available));
                    }
                }
            }
            "GPU" => {
                let values: Vec<_> = fields[2].split(',').map(str::trim).collect();
                if values.len() != 5 {
                    continue;
                }
                let Ok(index) = values[0].parse::<u32>() else {
                    continue;
                };
                let utilization = values[2]
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 100.0);
                let gpu = GpuUsage {
                    index,
                    name: values[1].into(),
                    utilization,
                    used_mib: values[3].parse().ok(),
                    total_mib: values[4].parse().ok(),
                };
                node.usage
                    .get_or_insert_with(|| NodeUsage {
                        ram: None,
                        gpus: Vec::new(),
                    })
                    .gpus
                    .push(gpu);
            }
            _ => {}
        }
    }
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
            name: name.into(),
            probe_requested: false,
            probe_error: None,
            usage: None,
            cpus: value(line, "CPUEfctv")
                .parse()
                .ok()
                .or_else(|| value(line, "CPUTot").parse().ok()),
            gpus: node_gpus(line),
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
    fn node_probe_errors_are_preserved() {
        let mut snapshot =
            parse("NodeName=n1 CPUAlloc=1 State=ALLOCATED Partitions=p", "").unwrap();
        snapshot.nodes[0].probe_requested = true;
        apply_usage(&mut snapshot, "n1|ERROR|SSH host key verification failed");
        assert_eq!(
            snapshot.nodes[0].probe_error.as_deref(),
            Some("SSH host key verification failed")
        );
        assert!(snapshot.nodes[0].usage.is_none());
        apply_usage(
            &mut snapshot,
            "n1|RAM|8388608|2097152\nn1|ERROR|node probe timed out",
        );
        assert_eq!(
            snapshot.nodes[0].usage.as_ref().unwrap().ram,
            Some((8388608, 2097152))
        );
        assert_eq!(
            snapshot.nodes[0].probe_error.as_deref(),
            Some("node probe timed out")
        );
    }

    #[test]
    fn parses_partial_node_telemetry_without_inventing_values() {
        let mut snapshot =
            parse("NodeName=n1 CPUAlloc=1 State=ALLOCATED Partitions=p", "").unwrap();
        snapshot.nodes[0].probe_requested = true;
        apply_usage(&mut snapshot, "n1|RAM|223080768|169829376\nn1|GPU|0, NVIDIA GH200 120GB, 93, 20456, 97871\nn1|GPU|1, other, N/A, N/A, 1024\nn2|RAM|100|10\nn1|RAM|10|20\nbanner");
        let usage = snapshot.nodes[0].usage.as_ref().unwrap();
        assert_eq!(usage.ram, Some((223080768, 169829376)));
        assert_eq!(usage.gpus[0].index, 0);
        assert_eq!(usage.gpus[1].index, 1);
        assert_eq!(usage.gpus[0].utilization, Some(93.0));
        assert_eq!(usage.gpus[0].used_mib, Some(20456));
        assert_eq!(usage.gpus[1].utilization, None);
        assert_eq!(usage.gpus[1].used_mib, None);
    }

    #[test]
    fn missing_gpu_fields_remain_unknown() {
        assert!(node_gpus(
            "Arch=aarch64 ActiveFeatures=arm,gh RealMemory=1 Gres=(null) State=ALLOCATED"
        )
        .is_empty());
    }

    #[test]
    fn gpu_allocation_falls_back_to_gres_used_without_overriding_tres() {
        assert_eq!(
            node_gpus("Gres=gpu:h100:4(S:0-1) GresUsed=gpu:h100:2(IDX:0,2)"),
            vec![GpuAllocation {
                model: "h100".into(),
                total: 4,
                allocated: Some(2)
            }]
        );
        assert_eq!(
            node_gpus("Gres=gpu:a100:4,gpu:h100:2 GresUsed=gpu:a100:1(IDX:0),gpu:h100:0(IDX:N/A)"),
            vec![
                GpuAllocation {
                    model: "a100".into(),
                    total: 4,
                    allocated: Some(1)
                },
                GpuAllocation {
                    model: "h100".into(),
                    total: 2,
                    allocated: Some(0)
                }
            ]
        );
        assert_eq!(
            node_gpus("Gres=gpu:4 GresUsed=gpu:2(IDX:0,1)"),
            vec![GpuAllocation {
                model: "".into(),
                total: 4,
                allocated: Some(2)
            }]
        );
        assert_eq!(
            node_gpus(
                "CfgTRES=gres/gpu:h100=4 AllocTRES=gres/gpu:h100=1 GresUsed=gpu:h100:2(IDX:0,1)"
            ),
            vec![GpuAllocation {
                model: "h100".into(),
                total: 4,
                allocated: Some(1)
            }]
        );
    }

    #[test]
    fn node_resources_preserve_unknowns_and_normalize_gpu_rollups() {
        let snapshot = parse("NodeName=n1 CPUAlloc=8 CPUTot=64 CPUEfctv=60 RealMemory=524288 AllocMem=131072 CfgTRES=cpu=64,gres/gpu=4,gres/gpu:h100=4 AllocTRES=cpu=8,gres/gpu=2 State=MIXED Partitions=p", "").unwrap();
        let node = &snapshot.nodes[0];
        assert_eq!(node.cpus, Some(60));
        assert_eq!(
            node.gpus,
            vec![GpuAllocation {
                model: "h100".into(),
                total: 4,
                allocated: Some(2)
            }]
        );
        let unknown = parse(
            "NodeName=n2 CPUAlloc=0 State=IDLE Partitions=p Gres=gpu:a100:4(S:0-1)",
            "",
        )
        .unwrap();
        assert_eq!(
            unknown.nodes[0].gpus,
            vec![GpuAllocation {
                model: "a100".into(),
                total: 4,
                allocated: None
            }]
        );
        assert_eq!(
            node_gpus("CfgTRES=gres/gpu=6,gres/gpu:a100=4,gres/gpu:h100=2 AllocTRES=gres/gpu=3"),
            vec![
                GpuAllocation {
                    model: "a100".into(),
                    total: 4,
                    allocated: None
                },
                GpuAllocation {
                    model: "h100".into(),
                    total: 2,
                    allocated: None
                }
            ]
        );
        assert_eq!(
            node_gpus("CfgTRES=gres/gpu=4 AllocTRES=cpu=2"),
            vec![GpuAllocation {
                model: "".into(),
                total: 4,
                allocated: Some(0)
            }]
        );
    }

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
