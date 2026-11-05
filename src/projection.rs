//! Estimate a partition's schedule using aggregate node counts.
//!
//! Running jobs occupy capacity until their estimated end. Pending jobs with Slurm
//! start estimates reserve capacity next; the remaining jobs are placed in input
//! queue order wherever their node count and time limit fit.
//!
//! Time is represented by 48 unequal buckets, with more detail near the present.
//! Relative times are seconds from `now`, except the duration parser's minutes.
//! A job occupies every bucket it touches, so placement is approximate. This does
//! not model individual nodes, CPUs, GPUs, memory, dependencies, or site policies.

use crate::slurm_parser::{Job, Snapshot};
use chrono::{DateTime, Local};

pub const TIME_BUCKETS: usize = 48;

/// Node occupancy for one time bucket; `own_*` counts belong to the displayed user.
#[derive(Clone, Copy, Default)]
pub struct Column {
    pub unavailable: u32,
    pub running: u32,
    pub own_running: u32,
    pub slurm: u32,
    pub own_slurm: u32,
    pub simulated: u32,
    pub own_simulated: u32,
}

impl Column {
    fn used(self) -> u32 {
        self.unavailable
            .saturating_add(self.running)
            .saturating_add(self.own_running)
            .saturating_add(self.slurm)
            .saturating_add(self.own_slurm)
            .saturating_add(self.simulated)
            .saturating_add(self.own_simulated)
    }
}

/// A pending job's estimated times; `slurm` distinguishes a supplied estimate from a local one.
#[derive(Clone, Debug)]
pub struct Placement {
    pub job_id: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub slurm: bool,
}

/// A vertical chart band in node-count units, not physical node identities.
#[derive(Clone, Copy, Debug)]
pub struct JobBand {
    pub start: u32,
    pub end: u32,
    pub color: u8,
}

#[derive(Clone, Copy)]
struct JobSpan {
    first: usize,
    last: usize,
    nodes: u32,
    color: u8,
}

// Personal jobs are stacked first within each estimate source.
#[derive(Default)]
struct OwnershipSpans {
    own: Vec<JobSpan>,
    other: Vec<JobSpan>,
}

impl OwnershipSpans {
    fn for_owner(&mut self, own: bool) -> &mut Vec<JobSpan> {
        if own {
            &mut self.own
        } else {
            &mut self.other
        }
    }
}

fn job_color(id: &str) -> u8 {
    (id.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    }) % 5) as u8
}

// Choose colors once per job so its shade stays consistent across time buckets.
fn job_bands(groups: &[Vec<JobSpan>], offset: u32) -> Vec<Vec<JobBand>> {
    let mut previous = None;
    let jobs: Vec<_> = groups
        .iter()
        .flatten()
        .map(|job| {
            let mut color = job.color;
            if previous == Some(color) {
                color = (color + 1) % 5;
            }
            previous = Some(color);
            (*job, color)
        })
        .collect();
    (0..TIME_BUCKETS)
        .map(|column| {
            let mut position = offset;
            jobs.iter()
                .filter(|(job, _)| job.first <= column && column < job.last && job.nodes > 0)
                .map(|(job, color)| {
                    let start = position;
                    position = position.saturating_add(job.nodes);
                    JobBand {
                        start,
                        end: position,
                        color: *color,
                    }
                })
                .collect()
        })
        .collect()
}

/// Capacity buckets, pending-job placements, and per-bucket bands for rendering.
pub struct Projection {
    pub columns: Vec<Column>,
    pub nodes: u32,
    /// Pending jobs without a usable duration or placement in this horizon.
    pub unresolved: usize,
    pub horizon: i64,
    pub placements: Vec<Placement>,
    pub running_bands: Vec<Vec<JobBand>>,
    pub pending_bands: Vec<Vec<JobBand>>,
    /// Latest known running or placed pending completion, in seconds from `now`; used to shrink the horizon.
    pub(crate) latest_end: i64,
}

// Round partial minutes upward; unknown or overflowing limits cannot be scheduled.
fn duration_minutes(value: &str) -> Option<i64> {
    if value == "UNLIMITED" || value == "NOT_SET" {
        return None;
    }
    let (days, time) = match value.split_once('-') {
        Some((days, time)) => (days.parse::<i64>().ok()?, time),
        None => (0, value),
    };
    let parts: Vec<i64> = time
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes] => minutes.checked_mul(60)?,
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    if days < 0 || seconds < 0 {
        return None;
    }
    days.checked_mul(1440)?
        .checked_add(seconds.checked_add(59)? / 60)
        .filter(|minutes| *minutes > 0)
}

/// Time-axis landmarks in seconds: 0, 1h, 3h, 6h, then successive doublings to the horizon.
pub fn time_anchors(horizon: i64) -> Vec<i64> {
    let mut anchors = vec![0];
    let mut tick = 3600_i64;
    while tick < horizon {
        anchors.push(tick);
        tick = if tick == 3600 {
            3 * 3600
        } else {
            tick.saturating_mul(2)
        };
        if tick == i64::MAX {
            break;
        }
    }
    anchors.push(horizon);
    anchors
}

/// Map landmarks to chart positions between 0 and 1.
/// Full landmark intervals get equal width; the final partial interval gets a proportional width.
pub fn time_positions(horizon: i64) -> Vec<(i64, f64)> {
    let anchors = time_anchors(horizon);
    let last = anchors.len() - 1;
    let previous = anchors[last - 1];
    let next = if previous == 0 {
        3600
    } else if previous == 3600 {
        3 * 3600
    } else {
        previous.saturating_mul(2)
    };
    let fraction = (horizon - previous) as f64 / (next - previous) as f64;
    let total = (last - 1) as f64 + fraction;
    anchors
        .into_iter()
        .enumerate()
        .map(|(index, time)| {
            (
                time,
                if index == last {
                    1.0
                } else {
                    index as f64 / total
                },
            )
        })
        .collect()
}

/// Interpolate the time axis at 49 edges to produce 48 bucket intervals.
pub fn time_boundaries(horizon: i64) -> [i64; TIME_BUCKETS + 1] {
    let positions = time_positions(horizon);
    let mut bounds = [0; TIME_BUCKETS + 1];
    for (column, bound) in bounds.iter_mut().enumerate() {
        let position = column as f64 / TIME_BUCKETS as f64;
        let segment = positions
            .partition_point(|(_, point)| *point <= position)
            .saturating_sub(1);
        if segment == positions.len() - 1 {
            *bound = horizon;
        } else {
            let (start, left) = positions[segment];
            let (end, right) = positions[segment + 1];
            *bound = start + ((end - start) as f64 * (position - left) / (right - left)) as i64;
        }
    }
    bounds
}

// Return the bucket slice covering [start, end); touching an end edge does not occupy the next bucket.
fn interval(start: i64, end: i64, bounds: &[i64; TIME_BUCKETS + 1]) -> Option<(usize, usize)> {
    if end <= 0 || start >= bounds[TIME_BUCKETS] || end <= start {
        return None;
    }
    let first = bounds
        .partition_point(|time| *time <= start)
        .saturating_sub(1);
    let last = bounds.partition_point(|time| *time < end).min(TIME_BUCKETS);
    (first < last).then_some((first, last))
}

impl Projection {
    /// Choose an automatic horizon (at least 48 hours), then build the projection.
    pub fn queue(snapshot: &Snapshot, partition: &str, now: DateTime<Local>) -> Self {
        let jobs = snapshot
            .jobs
            .iter()
            .filter(|job| partition.is_empty() || job.partition == partition);
        let mut latest_known_time = 0_i64;
        let mut duration = 0_i64;
        let mut pending = false;
        for job in jobs {
            if job.state == "RUNNING" || job.state == "COMPLETING" || job.state == "PENDING" {
                let time = if job.state == "PENDING" {
                    job.start
                } else {
                    job.end
                };
                if let Some(time) = time {
                    latest_known_time = latest_known_time.max((time - now).num_seconds());
                }
            }
            if job.state == "PENDING" {
                pending = true;
                if let Some(minutes) = duration_minutes(&job.time_limit) {
                    duration = duration.saturating_add(minutes.saturating_mul(60));
                }
            }
        }
        // shortcut: cap extremely long projections at 100 years; replace if longer queues need support.
        let horizon = if pending {
            latest_known_time
                .saturating_add(duration)
                .saturating_add(3600)
        } else {
            latest_known_time
        }
        .clamp(48 * 3600, 100 * 366 * 24 * 3600);
        // Start generously, then shrink; changed bucket widths can change which jobs fit.
        let mut projection = Self::build(snapshot, partition, horizon, now);
        for _ in 0..8 {
            let next_horizon =
                ((projection.latest_end.saturating_add(59)) / 60 * 60).max(48 * 3600);
            if next_horizon >= projection.horizon {
                break;
            }
            let next = Self::build(snapshot, partition, next_horizon, now);
            // Keep the wider horizon if shrinking would lose a previously placeable job.
            if next.unresolved > projection.unresolved {
                break;
            }
            projection = next;
        }
        projection
    }

    /// Project into a fixed horizon in seconds; an empty partition selects all data.
    pub fn build(snapshot: &Snapshot, partition: &str, horizon: i64, now: DateTime<Local>) -> Self {
        let nodes: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|n| partition.is_empty() || n.partitions.iter().any(|p| p == partition))
            .collect();
        let total = nodes.len() as u32;
        let unavailable = nodes
            .iter()
            .filter(|n| {
                n.state
                    .split('+')
                    .any(|s| !matches!(s, "IDLE" | "ALLOCATED" | "MIXED"))
            })
            .count() as u32;
        let allocated = nodes
            .iter()
            .filter(|n| {
                n.allocated > 0
                    && n.state
                        .split('+')
                        .all(|s| matches!(s, "IDLE" | "ALLOCATED" | "MIXED"))
            })
            .count() as u32;
        let jobs: Vec<&Job> = snapshot
            .jobs
            .iter()
            .filter(|j| partition.is_empty() || j.partition == partition)
            .collect();
        let running_nodes: u32 = jobs
            .iter()
            .filter(|j| j.state == "RUNNING" || j.state == "COMPLETING")
            .map(|j| j.nodes)
            .sum();
        // Keep unavailable nodes and allocations missing from the job list reserved throughout.
        let baseline = unavailable.saturating_add(allocated.saturating_sub(running_nodes));
        let columns = vec![
            Column {
                unavailable: baseline,
                ..Column::default()
            };
            TIME_BUCKETS
        ];
        let bounds = time_boundaries(horizon);
        let mut projection = Self {
            columns,
            nodes: total,
            unresolved: 0,
            horizon,
            latest_end: 0,
            placements: Vec::new(),
            running_bands: Vec::new(),
            pending_bands: Vec::new(),
        };
        projection.running_bands =
            projection.reserve_running(&jobs, &snapshot.viewer, &bounds, now, baseline);
        projection.pending_bands = projection.place_pending(&jobs, &snapshot.viewer, &bounds, now);
        projection
    }

    fn reserve_running(
        &mut self,
        jobs: &[&Job],
        viewer: &str,
        bounds: &[i64; TIME_BUCKETS + 1],
        now: DateTime<Local>,
        baseline: u32,
    ) -> Vec<Vec<JobBand>> {
        let mut spans = OwnershipSpans::default();
        for job in jobs
            .iter()
            .filter(|j| j.state == "RUNNING" || j.state == "COMPLETING")
        {
            let own = !viewer.is_empty() && job.user == viewer;
            let known_end = job
                .end
                .map(|end| (end - now).num_seconds())
                .filter(|end| *end > 0);
            self.latest_end = self.latest_end.max(known_end.unwrap_or(0));
            // A missing or expired estimate cannot release capacity while the job is still running.
            let end = known_end.unwrap_or(self.horizon);
            if let Some((first, last)) = interval(0, end, bounds) {
                spans.for_owner(own).push(JobSpan {
                    first,
                    last,
                    nodes: job.nodes,
                    color: job_color(&job.id),
                });
                for col in &mut self.columns[first..last] {
                    let target = if own {
                        &mut col.own_running
                    } else {
                        &mut col.running
                    };
                    *target = target.saturating_add(job.nodes);
                }
            }
        }
        job_bands(&[spans.own, spans.other], baseline)
    }

    fn place_pending(
        &mut self,
        jobs: &[&Job],
        viewer: &str,
        bounds: &[i64; TIME_BUCKETS + 1],
        now: DateTime<Local>,
    ) -> Vec<Vec<JobBand>> {
        let mut slurm_spans = OwnershipSpans::default();
        let mut local_spans = OwnershipSpans::default();
        // Slurm estimates reserve capacity first; remaining jobs use queue order and first fit.
        for estimated in [true, false] {
            for job in jobs
                .iter()
                .filter(|j| j.state == "PENDING" && j.start.is_some() == estimated)
            {
                let own = !viewer.is_empty() && job.user == viewer;
                let Some(duration) = duration_minutes(&job.time_limit) else {
                    self.unresolved += 1;
                    continue;
                };
                let length = duration.saturating_mul(60);
                let window = if let Some(start) = job.start {
                    let start = (start - now).num_seconds();
                    let end = start.saturating_add(length);
                    interval(start, end, bounds).map(|(first, last)| (first, last, end))
                } else {
                    self.first_available_window(job.nodes, length, bounds)
                };
                let Some((start, end, finish)) = window else {
                    self.unresolved += 1;
                    continue;
                };
                let spans = if estimated {
                    &mut slurm_spans
                } else {
                    &mut local_spans
                };
                spans.for_owner(own).push(JobSpan {
                    first: start,
                    last: end,
                    nodes: job.nodes,
                    color: job_color(&job.id),
                });
                self.latest_end = self.latest_end.max(finish);
                let start_time = if estimated {
                    job.start
                } else {
                    chrono::Duration::try_seconds(bounds[start])
                        .and_then(|d| now.checked_add_signed(d))
                };
                let end_time = start_time.and_then(|start| {
                    chrono::Duration::try_minutes(duration)
                        .and_then(|d| start.checked_add_signed(d))
                });
                if let (Some(start), Some(end)) = (start_time, end_time) {
                    self.placements.push(Placement {
                        job_id: job.id.clone(),
                        start,
                        end,
                        slurm: estimated,
                    });
                }
                for column in &mut self.columns[start..end] {
                    let target = if estimated {
                        if own {
                            &mut column.own_slurm
                        } else {
                            &mut column.slurm
                        }
                    } else if own {
                        &mut column.own_simulated
                    } else {
                        &mut column.simulated
                    };
                    *target = target.saturating_add(job.nodes);
                }
            }
        }
        job_bands(
            &[
                slurm_spans.own,
                slurm_spans.other,
                local_spans.own,
                local_spans.other,
            ],
            0,
        )
    }

    fn first_available_window(
        &self,
        nodes: u32,
        duration: i64,
        bounds: &[i64; TIME_BUCKETS + 1],
    ) -> Option<(usize, usize, i64)> {
        // Try bucket edges in order; capacity must fit across the entire job duration.
        for start in 0..TIME_BUCKETS {
            let finish = bounds[start].saturating_add(duration);
            if finish > bounds[TIME_BUCKETS] {
                continue;
            }
            let Some((first, last)) = interval(bounds[start], finish, bounds) else {
                continue;
            };
            let fits = self.columns[first..last]
                .iter()
                .all(|column| column.used().saturating_add(nodes) <= self.nodes);
            if fits {
                return Some((first, last, finish));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed time keeps scheduling assertions independent of when the tests run.
    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-11-05T12:00:00Z")
            .unwrap()
            .with_timezone(&Local)
    }

    // Most cases use two idle nodes in partition p and highlight user "me".
    fn snapshot(jobs: &str) -> Snapshot {
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p\nNodeName=n2 CPUAlloc=0 State=IDLE Partitions=p\n",
            jobs,
        ).unwrap();
        snapshot.viewer = "me".into();
        snapshot.collected = now();
        snapshot
    }

    #[test]
    fn durations_round_up_without_inventing_unknown_or_overflowing_limits() {
        for (input, expected) in [
            ("1", 1),
            ("00:01", 1),
            ("01:01", 2),
            ("01:00:01", 61),
            ("2-00:00:01", 2881),
        ] {
            assert_eq!(duration_minutes(input), Some(expected), "{input}");
        }
        for input in [
            "",
            "UNLIMITED",
            "NOT_SET",
            "N/A",
            "0",
            "00:00",
            "-1",
            "1:2:3:4",
            "9223372036854775807",
            "9223372036854775807-00:00:00",
        ] {
            assert_eq!(duration_minutes(input), None, "{input}");
        }
    }

    #[test]
    fn time_axis_is_monotone_and_covers_the_entire_horizon() {
        for horizon in [
            48 * 3600,
            48 * 3600 + 1,
            221 * 3600 + 300,
            100 * 366 * 24 * 3600,
        ] {
            let positions = time_positions(horizon);
            assert_eq!(positions.first(), Some(&(0, 0.0)));
            assert_eq!(positions.last(), Some(&(horizon, 1.0)));
            assert!(positions
                .windows(2)
                .all(|p| p[0].0 < p[1].0 && p[0].1 < p[1].1));
            let bounds = time_boundaries(horizon);
            assert_eq!(bounds[0], 0);
            assert_eq!(bounds[TIME_BUCKETS], horizon);
            assert!(bounds.windows(2).all(|p| p[0] < p[1]));
        }
    }

    #[test]
    fn intervals_clip_to_the_horizon_and_use_half_open_boundaries() {
        let bounds = time_boundaries(48 * 3600);
        for column in 0..TIME_BUCKETS {
            assert_eq!(
                interval(bounds[column], bounds[column + 1], &bounds),
                Some((column, column + 1))
            );
        }
        assert_eq!(interval(-100, bounds[1], &bounds), Some((0, 1)));
        assert_eq!(interval(1, bounds[1] + 1, &bounds), Some((0, 2)));
        assert_eq!(
            interval(bounds[TIME_BUCKETS - 1], bounds[TIME_BUCKETS] + 1, &bounds),
            Some((TIME_BUCKETS - 1, TIME_BUCKETS))
        );
        for (start, end) in [
            (-10, 0),
            (0, 0),
            (2, 1),
            (bounds[TIME_BUCKETS], bounds[TIME_BUCKETS] + 1),
        ] {
            assert_eq!(interval(start, end, &bounds), None, "[{start}, {end})");
        }
    }

    #[test]
    fn slurm_reservations_take_precedence_over_earlier_proxy_jobs() {
        let mut snapshot = snapshot(
            "proxy|me|p|PENDING|2|N/A|N/A|(null)|2:00:00|proxy|Resources\nslurm|other|p|PENDING|2|N/A|N/A|(null)|1:00:00|scheduled|Priority\n",
        );
        snapshot.jobs[1].start = Some(now() + chrono::Duration::hours(1));
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now());
        assert_eq!(projection.unresolved, 0);
        let scheduled = projection
            .placements
            .iter()
            .find(|p| p.job_id == "slurm")
            .unwrap();
        let proxy = projection
            .placements
            .iter()
            .find(|p| p.job_id == "proxy")
            .unwrap();
        assert!(scheduled.slurm);
        assert_eq!(scheduled.start, snapshot.jobs[1].start.unwrap());
        assert!(!proxy.slurm);
        assert!(proxy.start >= scheduled.end);
        assert_eq!(proxy.end - proxy.start, chrono::Duration::hours(2));
        assert!(projection
            .columns
            .iter()
            .all(|column| column.used() <= projection.nodes));
    }

    #[test]
    fn proxy_jobs_follow_queue_order_and_account_for_every_pending_job() {
        let snapshot = snapshot(
            "first|other|p|PENDING|2|N/A|N/A|(null)|1:00:00|first|Resources\nsecond|me|p|PENDING|2|N/A|N/A|(null)|1:00:00|second|Resources\nlarge|me|p|PENDING|3|N/A|N/A|(null)|1:00:00|large|Resources\nunknown|me|p|PENDING|1|N/A|N/A|(null)|UNLIMITED|unknown|Resources\nlong|me|p|PENDING|1|N/A|N/A|(null)|3-00:00:00|long|Resources\n",
        );
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now());
        assert_eq!(projection.unresolved, 3);
        assert_eq!(
            projection.placements.len() + projection.unresolved,
            snapshot.jobs.len()
        );
        assert_eq!(projection.placements[0].job_id, "first");
        assert_eq!(projection.placements[0].start, now());
        assert_eq!(projection.placements[1].job_id, "second");
        assert_eq!(projection.placements[1].start, projection.placements[0].end);
        assert_eq!(projection.columns[0].simulated, 2);
        assert_eq!(projection.columns[8].own_simulated, 2);
        assert!(projection
            .columns
            .iter()
            .all(|column| column.used() <= projection.nodes));
    }

    #[test]
    fn unavailable_and_unexplained_allocations_remain_reserved() {
        let mut snapshot = snapshot(
            "running|other|p|RUNNING|1|N/A|N/A|n1|1:00:00|running|None\npending|me|p|PENDING|2|N/A|N/A|(null)|1:00:00|pending|Resources\n",
        );
        snapshot.nodes[0].allocated = 4;
        snapshot.nodes[0].state = "ALLOCATED".into();
        snapshot.nodes[1].allocated = 2;
        snapshot.nodes[1].state = "MIXED".into();
        let mut down = snapshot.nodes[0].clone();
        down.state = "ALLOCATED+DRAIN".into();
        snapshot.nodes.push(down);
        snapshot.jobs[0].end = Some(now() + chrono::Duration::hours(1));
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now());
        assert_eq!(projection.nodes, 3);
        // One drained node plus one allocation absent from the job list.
        assert!(projection
            .columns
            .iter()
            .all(|column| column.unavailable == 2));
        assert_eq!(projection.columns[0].running, 1);
        assert_eq!(projection.columns[8].running, 0);
        assert_eq!(projection.unresolved, 1);
        assert!(projection.placements.is_empty());
    }

    #[test]
    fn partition_filter_excludes_foreign_capacity_jobs_and_horizon() {
        let mut snapshot = snapshot(
            "local|me|p|PENDING|2|N/A|N/A|(null)|1:00:00|local|Resources\nforeign|other|q|PENDING|100|N/A|N/A|(null)|UNLIMITED|foreign|Resources\n",
        );
        let mut foreign = snapshot.nodes[0].clone();
        foreign.partitions = vec!["q".into()];
        foreign.state = "DOWN".into();
        snapshot.nodes.push(foreign);
        snapshot.jobs[1].start = Some(now() + chrono::Duration::days(100));
        let projection = Projection::queue(&snapshot, "p", now());
        assert_eq!(projection.nodes, 2);
        assert_eq!(projection.horizon, 48 * 3600);
        assert_eq!(projection.unresolved, 0);
        assert_eq!(projection.placements.len(), 1);
        assert_eq!(projection.placements[0].job_id, "local");
        assert!(projection
            .columns
            .iter()
            .all(|column| column.unavailable == 0));
        let missing = Projection::queue(&snapshot, "missing", now());
        assert_eq!(missing.nodes, 0);
        assert!(missing.placements.is_empty());
        assert_eq!(missing.unresolved, 0);
    }

    #[test]
    fn changing_viewer_only_reclassifies_occupancy() {
        let mut snapshot = snapshot(
            "running|me|p|COMPLETING|1|N/A|N/A|n1|1:00:00|running|None\nslurm|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|scheduled|Priority\nproxy|me|p|PENDING|1|N/A|N/A|(null)|30:00|proxy|Resources\n",
        );
        snapshot.jobs[0].end = Some(now() + chrono::Duration::hours(1));
        snapshot.jobs[1].start = Some(now() + chrono::Duration::hours(1));
        let own = Projection::build(&snapshot, "p", 48 * 3600, now());
        snapshot.viewer.clear();
        let anonymous = Projection::build(&snapshot, "p", 48 * 3600, now());
        assert_eq!(own.unresolved, anonymous.unresolved);
        for (own, other) in own.columns.iter().zip(&anonymous.columns) {
            assert_eq!(own.used(), other.used());
            assert_eq!(own.own_running, other.running);
            assert_eq!(own.own_slurm, other.slurm);
            assert_eq!(own.own_simulated, other.simulated);
            assert_eq!(other.own_running + other.own_slurm + other.own_simulated, 0);
        }
        let placements = |projection: &Projection| {
            projection
                .placements
                .iter()
                .map(|p| (p.job_id.clone(), p.start, p.end, p.slurm))
                .collect::<Vec<_>>()
        };
        assert_eq!(placements(&own), placements(&anonymous));
    }

    #[test]
    fn neighboring_jobs_with_the_same_base_color_are_distinguished() {
        let spans = vec![
            JobSpan {
                first: 0,
                last: TIME_BUCKETS,
                nodes: 1,
                color: 2,
            },
            JobSpan {
                first: 0,
                last: TIME_BUCKETS,
                nodes: 2,
                color: 2,
            },
        ];
        let bands = job_bands(&[spans], 0);
        assert_ne!(bands[0][0].color, bands[0][1].color);
        assert_eq!(bands[0][1].color, bands[TIME_BUCKETS - 1][1].color);
    }

    #[test]
    fn parses_slurm_time_limits() {
        assert_eq!(duration_minutes("1-02:30:00"), Some(1590));
        assert_eq!(duration_minutes("30:00"), Some(30));
        assert_eq!(duration_minutes("UNLIMITED"), None);
    }
    #[test]
    fn nonlinear_axis_and_intervals() {
        let bounds = time_boundaries(48 * 3600);
        assert_eq!(bounds[8], 3600);
        assert_eq!(bounds[16], 3 * 3600);
        assert_eq!(bounds[48], 48 * 3600);
        assert_eq!(interval(3600, 3 * 3600, &bounds), Some((8, 16)));
        assert_eq!(time_boundaries(168 * 3600)[48], 168 * 3600);
        let horizon = 221 * 3600 + 5 * 60;
        let anchors = time_anchors(horizon);
        assert!(anchors.contains(&(96 * 3600)));
        assert!(anchors.contains(&(192 * 3600)));
        assert_eq!(anchors.last(), Some(&horizon));
        let points = time_positions(horizon);
        let penultimate = points[points.len() - 2].1;
        let prior = points[points.len() - 3].1;
        assert!(1.0 - penultimate < penultimate - prior);
        let bounds = time_boundaries(horizon);
        assert!(bounds.windows(2).all(|pair| pair[0] < pair[1]));
    }
    #[test]
    fn queue_horizon_tracks_projected_completion() {
        let now = Local::now();
        let node = "NodeName=n1 CPUTot=4 CPUAlloc=0 RealMemory=4096 AllocMem=0 State=IDLE Partitions=p Gres=(null) GresUsed=(null)\n";
        let pending = "1|me|p|PENDING|1|N/A|N/A|(null)|30:00|test-job|Resources\n";
        let mut snapshot = crate::slurm_parser::parse(node, pending).unwrap();
        let short = Projection::queue(&snapshot, "p", now);
        assert_eq!(short.horizon, 48 * 3600);
        assert_eq!(short.unresolved, 0);
        snapshot.jobs[0].start = Some(now + chrono::Duration::hours(200));
        let long = Projection::queue(&snapshot, "p", now);
        assert_eq!(long.horizon, (200 * 60 + 30) * 60);
        assert_eq!(long.unresolved, 0);
        assert!(long.placements[0].slurm);
        assert_eq!(long.placements[0].start, snapshot.jobs[0].start.unwrap());
        snapshot.jobs[0].time_limit = "UNLIMITED".into();
        let unknown = Projection::queue(&snapshot, "p", now);
        assert_eq!(unknown.unresolved, 1);
        assert_eq!(unknown.latest_end, 0);
    }
    #[test]
    fn smaller_job_fills_gap_before_larger_job() {
        let now = Local::now();
        let nodes = (1..=4).map(|i| format!("NodeName=n{i} CPUTot=4 CPUAlloc={} RealMemory=4096 AllocMem=0 State={} Partitions=p Gres=(null) GresUsed=(null)\n", if i <= 2 { 4 } else { 0 }, if i <= 2 { "ALLOCATED" } else { "IDLE" })).collect::<String>();
        let mut snapshot = crate::slurm_parser::parse(&nodes,
            "1|other|p|RUNNING|2|N/A|N/A|n[1-2]|1:00:00|test-job|Resources\n2|other|p|PENDING|3|N/A|N/A|(null)|1:00:00|test-job|Resources\n3|me|p|PENDING|1|N/A|N/A|(null)|30:00|test-job|Resources\n").unwrap();
        snapshot.viewer = "me".into();
        snapshot.jobs[0].end = Some(now + chrono::Duration::hours(1));
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now);
        assert_eq!(projection.columns[0].own_simulated, 1);
        assert_eq!(projection.columns[0].simulated, 0);
        assert_eq!(projection.columns[8].simulated, 3);
    }
    #[test]
    fn simulates_pending_after_running_ends() {
        let now = Local::now();
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=4 CPUAlloc=4 RealMemory=4096 AllocMem=1000 State=ALLOCATED Partitions=p Gres=(null) GresUsed=(null)\n",
            "1|other|p|RUNNING|1|N/A|N/A|n1|1:00:00|test-job|Resources\n2|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|test-job|Resources\n",
        ).unwrap();
        snapshot.viewer = "me".into();
        snapshot.jobs[0].end = Some(now + chrono::Duration::hours(1));
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now);
        assert_eq!(projection.columns[0].running, 1);
        assert_eq!(projection.columns[8].own_simulated, 1);
        let mine = projection
            .placements
            .iter()
            .find(|job| job.job_id == "2")
            .unwrap();
        assert!(!mine.slurm);
        assert!(mine.start >= snapshot.jobs[0].end.unwrap());
        assert_eq!(mine.end - mine.start, chrono::Duration::hours(1));
        assert_eq!(projection.unresolved, 0);
    }
    #[test]
    fn overdue_running_jobs_keep_their_capacity() {
        for state in ["RUNNING", "COMPLETING"] {
            for end in [
                Some(now() - chrono::Duration::minutes(5)),
                Some(now()),
                None,
            ] {
                let mut snapshot = snapshot(&format!(
                    "running|other|p|{state}|2|N/A|N/A|n[1-2]|1:00:00|running|None\npending|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|pending|Resources\n"
                ));
                snapshot.jobs[0].end = end;
                let projection = Projection::queue(&snapshot, "p", now());
                assert!(projection.columns.iter().all(|column| column.running == 2));
                assert!(projection.placements.is_empty());
                assert_eq!(projection.unresolved, 1, "{state}, end={end:?}");
            }
        }
    }

    #[test]
    fn running_completions_extend_the_horizon_even_with_short_pending_jobs() {
        for state in ["RUNNING", "COMPLETING"] {
            for pending in [
                "",
                "pending|me|p|PENDING|1|N/A|N/A|(null)|30:00|pending|Resources\n",
            ] {
                let mut snapshot = snapshot(&format!(
                    "running|other|p|{state}|1|N/A|N/A|n1|1:00:00|running|None\n{pending}"
                ));
                snapshot.jobs[0].end = Some(now() + chrono::Duration::hours(100));
                let projection = Projection::queue(&snapshot, "p", now());
                assert_eq!(projection.horizon, 100 * 3600);
                assert_eq!(projection.latest_end, 100 * 3600);
                assert_eq!(projection.unresolved, 0);
                assert!(projection.columns.iter().all(|column| column.running == 1));
            }
        }
    }
}
