use crate::{
    projection::{Column, Projection, TIME_BUCKETS},
    slurm_parser::Snapshot,
};
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

#[derive(Debug)]
pub struct Traffic {
    pub requests: u64,
    pub received: u64,
    started: std::time::Instant,
}

impl Default for Traffic {
    fn default() -> Self {
        Self {
            requests: 0,
            received: 0,
            started: std::time::Instant::now(),
        }
    }
}

impl Traffic {
    pub fn summary(&self, refresh_seconds: u64) -> String {
        let bytes = self.received;
        let rate = bytes as f64 / self.started.elapsed().as_secs_f64().max(1.0);
        format!(
            "SSH: {} req | RX {:.1} KiB | avg {:.0} B/s | poll {}s",
            self.requests,
            bytes as f64 / 1024.0,
            rate,
            refresh_seconds,
        )
    }
}

pub struct App {
    pub traffic: Traffic,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub loading: bool,
    pub estimate_dirty: bool,
    pub projection: Option<Projection>,
    pub tab: usize,
    pub editing_user: Option<String>,
    remote_user: String,
    account_override: Option<String>,
    host: String,
    partition: usize,
    own_scroll: usize,
    running_scroll: usize,
}

impl App {
    pub fn new(host: String) -> Self {
        Self {
            traffic: Traffic::default(),
            snapshot: None,
            error: None,
            loading: false,
            estimate_dirty: true,
            projection: None,
            tab: 0,
            editing_user: None,
            remote_user: String::new(),
            account_override: None,
            host,
            partition: 0,
            own_scroll: 0,
            running_scroll: 0,
        }
    }

    pub fn set_snapshot(&mut self, mut snapshot: Snapshot) {
        self.remote_user = snapshot.viewer.clone();
        if let Some(account) = &self.account_override {
            snapshot.viewer = account.clone();
        }
        self.snapshot = Some(snapshot);
        self.error = None;
        self.estimate_dirty = true;
    }

    fn partitions(&self) -> Vec<String> {
        let mut partitions: Vec<_> = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .nodes
                    .iter()
                    .flat_map(|node| {
                        node.partitions
                            .iter()
                            .filter(|partition| !partition.is_empty() && *partition != "(null)")
                            .cloned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        partitions.sort();
        partitions.dedup();
        if partitions.is_empty() {
            partitions.push(String::new());
        }
        partitions
    }

    pub fn estimate_input(&self) -> Option<(Snapshot, String)> {
        let partitions = self.partitions();
        let partition = partitions[self.partition % partitions.len()].clone();
        if partition.is_empty() {
            return None;
        }
        Some((self.snapshot.clone()?, partition))
    }

    pub fn key(&mut self, key: KeyCode) {
        if let Some(input) = &mut self.editing_user {
            match key {
                KeyCode::Esc => self.editing_user = None,
                KeyCode::Enter => {
                    let name = input.trim().to_owned();
                    self.account_override = (!name.is_empty()).then_some(name);
                    if let Some(snapshot) = &mut self.snapshot {
                        snapshot.viewer = self
                            .account_override
                            .clone()
                            .unwrap_or_else(|| self.remote_user.clone());
                    }
                    self.editing_user = None;
                    self.own_scroll = 0;
                    self.running_scroll = 0;
                    self.estimate_dirty = true;
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !c.is_control() && !c.is_whitespace() && input.len() < 128 => {
                    input.push(c)
                }
                _ => {}
            }
            return;
        }
        if matches!(key, KeyCode::Char('p')) {
            self.estimate_dirty = true;
        }
        match key {
            KeyCode::F(2) => {
                self.editing_user = Some(
                    self.snapshot
                        .as_ref()
                        .map(|s| s.viewer.clone())
                        .unwrap_or_default(),
                )
            }
            KeyCode::Char('1') => self.tab = 0,
            KeyCode::Char('2') => self.tab = 1,
            KeyCode::Char('p') => {
                self.partition = (self.partition + 1) % self.partitions().len();
            }
            KeyCode::Down if self.tab == 1 => {
                self.running_scroll = self.running_scroll.saturating_add(1)
            }
            KeyCode::Up if self.tab == 1 => {
                self.running_scroll = self.running_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.own_scroll = self.own_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => self.own_scroll = self.own_scroll.saturating_sub(1),
            _ => {}
        }
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
}

// Prefer a bucket containing your jobs when several buckets share a column.
fn chart_columns(projection: &Projection, width: usize) -> Vec<(Column, usize)> {
    (0..width)
        .map(|index| {
            let first = index * TIME_BUCKETS / width;
            let last = ((index + 1) * TIME_BUCKETS / width).max(first + 1);
            let range = &projection.columns[first..last];
            let selected = range
                .iter()
                .enumerate()
                .max_by_key(|(_, col)| {
                    let mine = col.own_running > 0 || col.own_slurm > 0 || col.own_simulated > 0;
                    let used = u64::from(col.unavailable)
                        + u64::from(col.own_running)
                        + u64::from(col.running)
                        + u64::from(col.own_slurm)
                        + u64::from(col.slurm)
                        + u64::from(col.own_simulated)
                        + u64::from(col.simulated);
                    (mine, used)
                })
                .map(|(index, _)| index)
                .unwrap_or(0);
            (range[selected], first + selected)
        })
        .collect()
}

fn plot(projection: &Projection, height: usize, width: usize) -> Vec<Line<'static>> {
    if projection.nodes == 0 {
        return vec![Line::from("No nodes in this partition")];
    }
    const RUNNING: [Color; 5] = [
        Color::Rgb(0, 211, 205),
        Color::Rgb(86, 170, 244),
        Color::Rgb(80, 218, 171),
        Color::Rgb(90, 130, 240),
        Color::Rgb(155, 221, 240),
    ];
    const SCHEDULED: [Color; 5] = [
        Color::Rgb(244, 93, 93),
        Color::Rgb(255, 141, 105),
        Color::Rgb(226, 88, 132),
        Color::Rgb(255, 166, 141),
        Color::Rgb(199, 89, 103),
    ];
    const PROXY: [Color; 5] = [
        Color::Rgb(192, 120, 235),
        Color::Rgb(155, 115, 230),
        Color::Rgb(220, 140, 235),
        Color::Rgb(150, 85, 190),
        Color::Rgb(190, 150, 245),
    ];
    const OWN: [Color; 5] = [
        Color::Rgb(120, 245, 140),
        Color::Rgb(255, 220, 100),
        Color::Rgb(110, 230, 255),
        Color::Rgb(255, 165, 100),
        Color::Rgb(245, 150, 220),
    ];
    let mut lines = Vec::new();
    let columns = chart_columns(projection, width);
    let height = height.max(1);
    let margin = format!("({} nodes)", projection.nodes).len().max(4);
    for row in (0..height).rev() {
        let tick = if row == height - 1 {
            "100%"
        } else if row == height / 2 {
            "50%"
        } else if row == height / 4 {
            "25%"
        } else {
            ""
        };
        let mut spans = vec![Span::raw(format!("{tick:>margin$} │"))];
        let threshold =
            (((2 * row as u64 + 1) * projection.nodes as u64) / (2 * height as u64)) as u32;
        for (col, source_index) in &columns {
            let segments = [
                (col.unavailable, '×', Color::Gray),
                (col.own_running, '◆', Color::LightGreen),
                (col.running, '█', Color::Cyan),
                (col.own_slurm, '◆', Color::LightGreen),
                (col.slurm, '▓', Color::Red),
                (col.own_simulated, '◆', Color::LightGreen),
                (col.simulated, '▒', Color::Magenta),
            ];
            let mut boundary = 0_u32;
            let mut cell = (' ', Color::Reset);
            for (count, symbol, color) in segments {
                boundary = boundary.saturating_add(count);
                if threshold < boundary {
                    cell = (symbol, color);
                    break;
                }
            }
            let own_ranges = [
                (col.unavailable, col.own_running),
                (
                    col.unavailable
                        .saturating_add(col.own_running)
                        .saturating_add(col.running),
                    col.own_slurm,
                ),
                (
                    col.unavailable
                        .saturating_add(col.own_running)
                        .saturating_add(col.running)
                        .saturating_add(col.own_slurm)
                        .saturating_add(col.slurm),
                    col.own_simulated,
                ),
            ];
            let own_marker = own_ranges
                .iter()
                .enumerate()
                .find_map(|(source, &(start, count))| {
                    if count == 0 || start >= projection.nodes {
                        return None;
                    }
                    let end = start.saturating_add(count).min(projection.nodes);
                    let (bands, offset) = if source == 0 {
                        (&projection.running_bands, 0)
                    } else {
                        (
                            &projection.pending_bands,
                            col.unavailable
                                .saturating_add(col.own_running)
                                .saturating_add(col.running),
                        )
                    };
                    let marker = |start: u32, end: u32| {
                        let midpoint = u64::from(start) + u64::from(end) - 1;
                        midpoint * height as u64 / (2 * u64::from(projection.nodes)) == row as u64
                    };
                    // Keep small jobs visible individually rather than marking their combined range.
                    if let Some(bands) = bands.get(*source_index) {
                        bands.iter().find_map(|band| {
                            let left = band.start.saturating_add(offset).max(start);
                            let right = band.end.saturating_add(offset).min(end);
                            (left < right && marker(left, right))
                                .then_some(OWN[usize::from(band.color)])
                        })
                    } else {
                        marker(start, end).then_some(Color::LightGreen)
                    }
                });
            if let Some(color) = own_marker {
                cell = ('◆', color);
            }
            if own_marker.is_none() && !matches!(cell.0, ' ' | '×') {
                let running_size = col
                    .unavailable
                    .saturating_add(col.own_running)
                    .saturating_add(col.running);
                let scheduled_size = col.own_slurm.saturating_add(col.slurm);
                let (bands, position, colors) = if threshold < running_size {
                    (&projection.running_bands, threshold, &RUNNING)
                } else if threshold < running_size.saturating_add(scheduled_size) {
                    (
                        &projection.pending_bands,
                        threshold - running_size,
                        &SCHEDULED,
                    )
                } else {
                    (&projection.pending_bands, threshold - running_size, &PROXY)
                };
                if let Some(band) = bands.get(*source_index).and_then(|bands| {
                    bands
                        .get(bands.partition_point(|band| band.end <= position))
                        .filter(|band| band.start <= position)
                }) {
                    cell.1 = if cell.0 == '◆' {
                        OWN[usize::from(band.color)]
                    } else {
                        colors[usize::from(band.color)]
                    };
                }
            }
            spans.push(Span::styled(
                cell.0.to_string(),
                Style::default().fg(cell.1),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(format!("{:>margin$} └", "0%")));
    lines
}

fn time_axis(horizon: i64, width: usize) -> String {
    let positions = crate::projection::time_positions(horizon);
    let labels: Vec<_> = positions
        .iter()
        .map(|(seconds, _)| {
            if *seconds == 0 {
                "now".to_owned()
            } else {
                let minutes = seconds / 60;
                if minutes % 60 == 0 {
                    format!("{}h", minutes / 60)
                } else if minutes < 60 {
                    format!("{minutes}m")
                } else {
                    format!("{}h{}m", minutes / 60, minutes % 60)
                }
            }
        })
        .collect();
    let mut axis = vec![' '; width + labels.last().map_or(0, |label| label.len())];
    let mut previous_end = 0;
    for (index, label) in labels.iter().enumerate() {
        let position = (positions[index].1 * width as f64).round() as usize;
        if index > 0
            && index + 1 < labels.len()
            && (position < previous_end + 1 || position + label.len() + 1 > width)
        {
            continue;
        }
        if index + 1 == labels.len() && position < previous_end {
            continue;
        }
        for (offset, character) in label.chars().enumerate() {
            if position + offset < axis.len() {
                axis[position + offset] = character;
            }
        }
        previous_end = position + label.len();
    }
    axis.into_iter().collect()
}

fn chart(projection: Option<&Projection>, height: u16, width: u16) -> Vec<Line<'static>> {
    match projection {
        Some(projection) => {
            let graph_height = height.saturating_sub(4) as usize;
            let node_count = format!("({} nodes)", projection.nodes);
            let margin = node_count.len().max(4);
            let final_label = time_axis(projection.horizon, 0).len();
            let plot_width =
                usize::from(width.saturating_sub((margin + 4 + final_label) as u16)).max(1);
            let mut lines = plot(projection, graph_height, plot_width);
            lines.push(Line::from(format!(
                "{node_count:>margin$}  {}",
                time_axis(projection.horizon, plot_width)
            )));
            lines
        }
        None => vec![Line::from("Calculating capacity…")],
    }
}

fn own_schedule<'a>(
    jobs: &[&crate::slurm_parser::Job],
    projection: Option<&Projection>,
    scroll: usize,
    height: u16,
) -> Vec<Line<'a>> {
    if jobs.is_empty() {
        return vec![Line::from("No active jobs")];
    }
    let visible = (height.saturating_sub(2) as usize / 3).max(1);
    let offset = scroll.min(jobs.len().saturating_sub(visible));
    let mut lines = Vec::new();
    for job in jobs.iter().skip(offset).take(visible) {
        let state = match job.state.as_str() {
            "RUNNING" => "running",
            "COMPLETING" => "completing",
            _ => "pending",
        };
        lines.push(Line::from(format!("{}  {}", job.id, state)));
        let placement = projection.and_then(|p| p.placements.iter().find(|p| p.job_id == job.id));
        let (start, end) = if matches!(job.state.as_str(), "RUNNING" | "COMPLETING") {
            (
                job.start
                    .map(|time| format!("Start {}", time.format("%m/%d %H:%M")))
                    .unwrap_or_else(|| "Start unknown".into()),
                job.end,
            )
        } else if let Some(placement) = placement {
            (
                format!(
                    "{} {}",
                    if placement.slurm { "Slurm" } else { "Proxy" },
                    placement.start.format("%m/%d %H:%M")
                ),
                Some(placement.end),
            )
        } else if let Some(start) = job.start {
            (format!("Slurm {}", start.format("%m/%d %H:%M")), None)
        } else {
            ("Start unknown".into(), None)
        };
        lines.push(Line::from(Span::styled(
            start,
            Style::default().fg(Color::LightGreen),
        )));
        let end = end
            .map(|time| format!("End {}", time.format("%m/%d %H:%M")))
            .unwrap_or_else(|| "End unknown".into());
        lines.push(Line::from(end));
    }
    lines
}

fn cpu_measurement(node: Option<&crate::slurm_parser::CpuNode>, bar_width: usize) -> String {
    let Some(node) = node else {
        return format!("{:>6} {:>4}  unavailable", "N/A", "N/A");
    };
    let Some(load) = node.load.filter(|_| node.total > 0) else {
        return format!("{:>6} {:>4}  unavailable", "N/A", node.total);
    };
    let ratio = load / f64::from(node.total);
    let bar = usage_bar(ratio, bar_width);
    format!("{load:>6.2} {:>4}{bar} {:>6.1}%", node.total, ratio * 100.0)
}

fn usage_bar(ratio: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = (ratio * width as f64).round().clamp(0.0, width as f64) as usize;
    format!(" [{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

fn usage_line(label: String, ratio: Option<f64>, width: usize, color: Color) -> Line<'static> {
    let bar_width = width.saturating_sub(label.chars().count() + 3).min(20);
    let mut spans = vec![Span::styled(label, Style::default().fg(color))];
    if let Some(ratio) = ratio
        .filter(|v| v.is_finite() && *v >= 0.0)
        .filter(|_| bar_width > 0)
    {
        let filled = (ratio * bar_width as f64)
            .round()
            .clamp(0.0, bar_width as f64) as usize;
        spans.extend([
            Span::styled(" [", Style::default().fg(color)),
            Span::styled(
                "#".repeat(filled),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-".repeat(bar_width - filled),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("]", Style::default().fg(color)),
        ]);
    }
    Line::from(spans)
}

fn resource_rows(node: &crate::slurm_parser::Node, width: usize) -> Vec<Line<'static>> {
    let cpu = node
        .cpus
        .map(|total| format!("{}/{total}", node.allocated))
        .unwrap_or_else(|| "N/A".into());
    let mut lines = vec![metric_line(format!("  Allocated CPU {cpu}"), Color::Gray)];
    for gpu in &node.gpus {
        let (model, total, used) = (&gpu.model, gpu.total, gpu.allocated);
        let label = if model.is_empty() { "GPU" } else { model };
        let used = used
            .map(|count| count.to_string())
            .unwrap_or_else(|| "?".into());
        lines.push(metric_line(
            format!("  Allocated GPU {label} {used}/{total}"),
            Color::Gray,
        ));
    }
    if node.gpus.is_empty() {
        lines.push(metric_line("  Allocated GPU N/A".into(), WARNING_COLOR));
    }
    if let Some(usage) = &node.usage {
        if let Some((total, available)) = usage.ram {
            lines.push(usage_line(
                format!(
                    "  RAM used {:.1}/{:.1} GiB",
                    (total - available) as f64 / 1048576.0,
                    total as f64 / 1048576.0
                ),
                (total > 0).then(|| (total - available) as f64 / total as f64),
                width,
                RAM_COLOR,
            ));
        } else {
            lines.push(metric_line("  RAM usage unavailable".into(), WARNING_COLOR));
        }
        for gpu in &usage.gpus {
            let utilization = gpu
                .utilization
                .map(|value| format!("{value:.0}%"))
                .unwrap_or_else(|| "N/A".into());
            let memory = |value: Option<u64>| {
                value
                    .map(|v| format!("{:.1}", v as f64 / 1024.0))
                    .unwrap_or_else(|| "?".into())
            };
            lines.push(usage_line(
                format!("  GPU {} {} busy {utilization}", gpu.index, gpu.name),
                gpu.utilization.map(|v| v / 100.0),
                width,
                if gpu.utilization.is_some() {
                    GPU_COLOR
                } else {
                    WARNING_COLOR
                },
            ));
            let ratio = gpu
                .used_mib
                .zip(gpu.total_mib)
                .filter(|(_, total)| *total > 0)
                .map(|(used, total)| used as f64 / total as f64);
            lines.push(usage_line(
                format!(
                    "  VRAM used {}/{} GiB",
                    memory(gpu.used_mib),
                    memory(gpu.total_mib)
                ),
                ratio,
                width,
                if ratio.is_some() {
                    VRAM_COLOR
                } else {
                    WARNING_COLOR
                },
            ));
        }
        if let Some(error) = &node.probe_error {
            lines.push(metric_line(
                format!("  Partial usage ({error})"),
                WARNING_COLOR,
            ));
        }
        if usage.gpus.is_empty() && !node.gpus.is_empty() {
            lines.push(metric_line("  GPU usage unavailable".into(), WARNING_COLOR));
        }
    } else if node.probe_requested {
        lines.push(metric_line(
            format!(
                "  Usage unavailable ({})",
                node.probe_error
                    .as_deref()
                    .unwrap_or("node probe failed/timed out")
            ),
            WARNING_COLOR,
        ));
    }
    let mut packed: Vec<Line<'static>> = Vec::new();
    for mut line in lines {
        if let Some(last) = packed
            .last_mut()
            .filter(|last| last.width() + 3 + line.width() <= width)
        {
            last.spans
                .push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            if let Some(first) = line.spans.first_mut() {
                first.content = first.content.trim_start().to_owned().into();
            }
            last.spans.extend(line.spans);
        } else {
            packed.push(line);
        }
    }
    packed
}

#[cfg(test)]
fn resource_lines(node: &crate::slurm_parser::Node, width: usize) -> Vec<String> {
    resource_rows(node, width)
        .into_iter()
        .map(|line| line.to_string())
        .collect()
}

const CPU_COLOR: Color = Color::Rgb(137, 220, 235);
const RAM_COLOR: Color = Color::Rgb(137, 180, 250);
const GPU_COLOR: Color = Color::Rgb(166, 227, 161);
const VRAM_COLOR: Color = Color::Rgb(203, 166, 247);
const WARNING_COLOR: Color = Color::Rgb(249, 226, 175);

fn metric_line(text: String, color: Color) -> Line<'static> {
    Line::styled(text, Style::default().fg(color))
}

fn draw_job_monitor(
    frame: &mut Frame,
    app: &App,
    snapshot: &Snapshot,
    jobs: &[&crate::slurm_parser::Job],
    panels: [ratatui::layout::Rect; 2],
) -> (usize, usize) {
    let running: Vec<_> = jobs
        .iter()
        .filter(|job| matches!(job.state.as_str(), "RUNNING" | "COMPLETING"))
        .collect();
    let pending: Vec<_> = jobs.iter().filter(|job| job.state == "PENDING").collect();
    let block = panel(" Running · node load / allocation · ↑/↓ ");
    let inner = block.inner(panels[0]);
    let mut running_lines = Vec::new();
    if running.is_empty() {
        running_lines.push(Line::from("No running jobs for this account"));
    } else if let Some(cpu) = &snapshot.cpu {
        let nodes: std::collections::HashMap<_, _> =
            cpu.iter().map(|node| (node.name.as_str(), node)).collect();
        running_lines.push(Line::styled(
            "Node-wide; includes other users",
            Style::default().fg(Color::Gray),
        ));
        let mut unique_hosts = std::collections::HashSet::new();
        for job in &running {
            if let Some(hosts) = crate::slurm_parser::hosts(&job.node_list) {
                unique_hosts.extend(hosts);
            }
        }
        let reported: Vec<_> = unique_hosts
            .iter()
            .filter_map(|host| nodes.get(host.as_str()))
            .collect();
        let allocated: u64 = reported.iter().map(|node| u64::from(node.allocated)).sum();
        let idle: u64 = reported.iter().map(|node| u64::from(node.idle)).sum();
        let other: u64 = reported.iter().map(|node| u64::from(node.other)).sum();
        let total: u64 = reported.iter().map(|node| u64::from(node.total)).sum();
        running_lines.push(Line::from(format!(
            "Allocated {allocated}/{total} · idle {idle} · other {other}"
        )));
        let bar_width = usize::from(inner.width).saturating_sub(33).min(30);
        for job in &running {
            running_lines.push(Line::from(Span::styled(
                format!("Job {}: {}", job.id, job.name),
                Style::default().fg(Color::Cyan),
            )));
            running_lines.push(Line::styled(
                "NODE         LOAD CPUS  LOAD / CPUS",
                Style::default().fg(CPU_COLOR),
            ));
            if let Some(hosts) = crate::slurm_parser::hosts(&job.node_list) {
                let mut hosts: Vec<_> = hosts.into_iter().collect();
                hosts.sort();
                for host in hosts {
                    let measurement = cpu_measurement(nodes.get(host.as_str()).copied(), bar_width);
                    let resources = snapshot.nodes.iter().find(|node| node.name == host);
                    // Long hostnames get their own line so their full identity remains visible.
                    if host.chars().count() > 10 {
                        running_lines.push(Line::from(host));
                        running_lines
                            .push(metric_line(format!("           {measurement}"), CPU_COLOR));
                    } else {
                        running_lines
                            .push(metric_line(format!("{host:<10} {measurement}"), CPU_COLOR));
                    }
                    if let Some(node) = resources {
                        running_lines.extend(resource_rows(node, usize::from(inner.width)));
                    }
                }
            } else {
                running_lines.push(Line::from(format!(
                    "Cannot expand nodes: {}",
                    job.node_list
                )));
            }
        }
    } else {
        running_lines.push(Line::from("Waiting for sinfo; press r to retry"));
    }
    let running_offset = app
        .running_scroll
        .min(running_lines.len().saturating_sub(inner.height as usize));
    frame.render_widget(
        Paragraph::new(running_lines)
            .block(block)
            .scroll((running_offset.min(u16::MAX as usize) as u16, 0)),
        panels[0],
    );
    let block = panel(" Pending jobs · j/k scroll ");
    let visible = block.inner(panels[1]).height as usize / 3;
    let offset = app.own_scroll.min(pending.len().saturating_sub(1));
    let mut lines = Vec::new();
    for job in pending.iter().skip(offset).take(visible.max(1)) {
        lines.push(Line::from(Span::styled(
            format!("{}  {}  {} nodes", job.id, job.partition, job.nodes),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(format!("Reason: {}", job.reason)));
        let start = job
            .start
            .map(|t| format!("Slurm: {}", t.format("%m/%d %H:%M")))
            .or_else(|| {
                app.projection
                    .as_ref()
                    .and_then(|p| p.placements.iter().find(|p| p.job_id == job.id))
                    .map(|p| format!("Proxy: {}", p.start.format("%m/%d %H:%M")))
            })
            .unwrap_or_else(|| "Start: unknown".into());
        lines.push(Line::from(start));
    }
    if pending.is_empty() {
        lines.push(Line::from("No pending jobs for this account"));
    }
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        panels[1],
    );
    (running_offset, offset)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let status = if app.loading {
        "refreshing".into()
    } else if let Some(error) = &app.error {
        format!("ERROR: {error}")
    } else if let Some(snapshot) = &app.snapshot {
        format!("updated {}", snapshot.collected.format("%H:%M:%S"))
    } else {
        "waiting for data".into()
    };
    let tab_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let tabs = if app.tab == 0 {
        vec![
            Span::styled("[1 Schedule]", tab_style),
            Span::raw("  2 Jobs"),
        ]
    } else {
        vec![
            Span::raw("1 Schedule  "),
            Span::styled("[2 Jobs]", tab_style),
        ]
    };
    let mut header = tabs;
    header.push(Span::raw(format!(
        "  |  {}  |  {}  |  F2 account",
        app.host, status
    )));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(header),
            Line::from(app.traffic.summary(crate::REFRESH_SECONDS)).right_aligned(),
        ]),
        areas[0],
    );
    let partitions = app.partitions();
    app.partition %= partitions.len();
    let partition = &partitions[app.partition];
    let Some(snapshot) = &app.snapshot else {
        let message = if let Some(error) = &app.error {
            format!("Unable to load Slurm data: {error}\n\nPress r to retry or q to quit.")
        } else {
            "Connecting to Slurm over SSH…\n\nPress q to quit.".into()
        };
        frame.render_widget(
            Paragraph::new(message)
                .block(panel(" Slurm connection "))
                .wrap(Wrap { trim: true }),
            areas[2],
        );
        return;
    };
    if partition.is_empty() {
        if let Some(input) = &app.editing_user {
            frame.render_widget(
                Paragraph::new(format!(
                    "F2 account: {input}▏  Enter apply · Esc cancel · empty = remote login"
                )),
                areas[1],
            );
        }
        frame.render_widget(
            Paragraph::new("No partitions reported by Slurm").block(panel(" Partitions ")),
            areas[2],
        );
        return;
    }
    let projection = app.projection.as_ref();
    let name = partition;
    let viewer = if snapshot.viewer.is_empty() {
        "unknown"
    } else {
        &snapshot.viewer
    };
    let jobs: Vec<_> = snapshot
        .jobs
        .iter()
        .filter(|job| {
            !snapshot.viewer.is_empty()
                && job.user == snapshot.viewer
                && (partition.is_empty() || job.partition == *partition)
                && (matches!(job.state.as_str(), "RUNNING" | "COMPLETING")
                    || job.state == "PENDING")
        })
        .collect();
    let unresolved = projection.map_or(0, |p| p.unresolved);
    let node_count = snapshot
        .nodes
        .iter()
        .filter(|node| partition.is_empty() || node.partitions.contains(partition))
        .count();
    let detail = if app.tab == 1 {
        let running = jobs
            .iter()
            .filter(|job| matches!(job.state.as_str(), "RUNNING" | "COMPLETING"))
            .count();
        let pending = jobs.iter().filter(|job| job.state == "PENDING").count();
        format!("{running} running · {pending} pending  |  node totals include other users")
    } else {
        format!("Blank = no jobs shown; not necessarily free  |  {unresolved} pending unprojected")
    };
    let summary = format!(
        "Partition: {name}  |  {node_count} nodes  |  user: {viewer}  |  your jobs: {}\n{detail}",
        jobs.len()
    );
    let summary = if let Some(input) = &app.editing_user {
        format!("F2 account: {input}▏  Enter apply · Esc cancel · empty = remote login")
    } else {
        summary
    };
    frame.render_widget(Paragraph::new(summary).wrap(Wrap { trim: true }), areas[1]);
    const MAX_CHART_HEIGHT: u16 = 28;
    let (mut graphs_area, mut detail_area) = if areas[2].width >= 60 {
        let sidebar = 24.min(areas[2].width.saturating_sub(38));
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(38), Constraint::Length(sidebar)])
            .split(areas[2]);
        (columns[0], columns[1])
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(
                    ((u32::from(areas[2].height) * 55 / 100) as u16).min(MAX_CHART_HEIGHT),
                ),
                Constraint::Min(0),
            ])
            .split(areas[2]);
        (rows[0], rows[1])
    };
    graphs_area.height = graphs_area.height.min(MAX_CHART_HEIGHT);
    detail_area.height = detail_area.height.min(MAX_CHART_HEIGHT);
    let mut legend_area = areas[3];
    legend_area.y = graphs_area.bottom().max(detail_area.bottom());
    let mut instructions_area = areas[4];
    instructions_area.y = legend_area.bottom();
    if app.tab == 1 {
        let (running_offset, pending_offset) =
            draw_job_monitor(frame, app, snapshot, &jobs, [graphs_area, detail_area]);
        app.running_scroll = running_offset;
        app.own_scroll = pending_offset;
        frame.render_widget(
            Paragraph::new("Node-wide: load ≠ CPU busy; allocations ≠ measured RAM/GPU usage"),
            legend_area,
        );
        frame.render_widget(
            Paragraph::new(
                "1 schedule  2 jobs  F2 account  ↑/↓ running  j/k pending  r refresh  q quit",
            ),
            instructions_area,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(chart(projection, graphs_area.height, graphs_area.width))
            .block(panel(" Running + scheduled + proxy (nodes) ")),
        graphs_area,
    );
    let visible = (detail_area.height.saturating_sub(2) as usize / 3).max(1);
    app.own_scroll = app.own_scroll.min(jobs.len().saturating_sub(visible));
    let scroll = app.own_scroll;
    frame.render_widget(
        Paragraph::new(own_schedule(&jobs, projection, scroll, detail_area.height))
            .block(panel(" Your schedule "))
            .wrap(Wrap { trim: true }),
        detail_area,
    );
    let legend = Line::from(vec![
        Span::styled("█ run", Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(
            "▓ Slurm scheduled",
            Style::default().fg(Color::Rgb(244, 93, 93)),
        ),
        Span::raw("  "),
        Span::styled(
            "▒ proxy schedule",
            Style::default().fg(Color::Rgb(192, 120, 235)),
        ),
        Span::raw("  "),
        Span::raw("colors=jobs  "),
        Span::styled("◆ yours", Style::default().fg(Color::LightGreen)),
        Span::raw("  × unavailable"),
    ]);
    frame.render_widget(Paragraph::new(legend), legend_area);
    frame.render_widget(
        Paragraph::new(
            "1 Schedule  2 Jobs  F2 Account  p Partition  j/k Scroll  r Refresh  q Quit",
        )
        .wrap(Wrap { trim: true }),
        instructions_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn metric_styles_do_not_depend_on_label_text() {
        let label = "RAM [device] busy N/A";
        let line = usage_line(label.into(), Some(0.5), 80, GPU_COLOR);
        assert_eq!(line.spans[0].content, label);
        assert_eq!(line.spans[0].style.fg, Some(GPU_COLOR));
        assert!(line
            .spans
            .iter()
            .any(|span| span.style.fg == Some(Color::DarkGray)));
        assert!(line.to_string().starts_with(label));
    }

    #[test]
    fn usage_bars_scale_and_fit_available_width() {
        assert_eq!(usage_bar(0.0, 4), " [----]");
        assert_eq!(usage_bar(0.5, 4), " [##--]");
        assert_eq!(usage_bar(1.5, 4), " [####]");
        assert_eq!(
            usage_line("RAM".into(), None, 40, RAM_COLOR).to_string(),
            "RAM"
        );
        assert_eq!(
            usage_line("RAM".into(), Some(0.5), 10, RAM_COLOR).to_string(),
            "RAM [##--]"
        );
        assert_eq!(
            usage_line("RAM".into(), Some(0.5), 3, RAM_COLOR).to_string(),
            "RAM"
        );
    }

    #[test]
    fn measured_usage_display_and_probe_failure() {
        let mut snapshot =
            crate::slurm_parser::parse("NodeName=n1 CPUAlloc=1 State=ALLOCATED Partitions=p", "")
                .unwrap();
        snapshot.nodes[0].probe_requested = true;
        assert!(resource_lines(&snapshot.nodes[0], 160)
            .join(" ")
            .contains("node probe failed/timed out"));
        crate::slurm_parser::apply_usage(
            &mut snapshot,
            "n1|RAM|8388608|2097152\nn1|GPU|0, NVIDIA GH200, 93, 20480, 98304",
        );
        for width in [40, 160] {
            let lines = resource_lines(&snapshot.nodes[0], width);
            let text = lines.join(" ");
            assert!(text.contains("RAM used 6.0/8.0 GiB"));
            assert!(text.contains("GPU 0 NVIDIA GH200 busy 93%"));
            assert!(text.contains("VRAM used 20.0/96.0 GiB"));
            assert!(!text.contains("probe failed"));
        }
    }

    #[test]
    fn vista_display_omits_ram_allocation_and_free() {
        let snapshot = crate::slurm_parser::parse("NodeName=n1 Arch=aarch64 CPUAlloc=72 ActiveFeatures=arm,gh Gres=(null) RealMemory=1 AllocMem=0 FreeMem=135552 State=ALLOCATED Partitions=gh", "").unwrap();
        let lines = resource_lines(&snapshot.nodes[0], 160).join("\n");
        assert!(!lines.contains("RAM free"));
        assert!(!lines.contains("Allocated RAM"));
        assert!(lines.contains("Allocated GPU N/A"));
        assert!(!lines.contains("0.0/0.0"));
    }

    #[test]
    fn wide_rows_pack_resources() {
        let snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUAlloc=2 CPUTot=8 RealMemory=32768 State=MIXED Partitions=p",
            "",
        )
        .unwrap();
        let mut node = snapshot.nodes[0].clone();
        node.gpus = vec![crate::slurm_parser::GpuAllocation {
            model: "h100".into(),
            total: 4,
            allocated: Some(2),
        }];
        let narrow = resource_lines(&node, 40);
        let wide = resource_lines(&node, 160);
        assert_eq!(wide.len(), 1);
        assert!(wide[0].contains("CPU 2/8 · Allocated GPU h100 2/4"));
        assert!(narrow.len() > wide.len());
        assert!(narrow.iter().all(|line| line.chars().count() <= 40));
    }

    #[test]
    fn resource_display_shows_reservations_at_narrow_and_wide_sizes() {
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=64 CPUAlloc=32 RealMemory=524288 AllocMem=131072 CfgTRES=gres/gpu=4,gres/gpu:h100=4 AllocTRES=gres/gpu=2 State=MIXED Partitions=p",
            "42|me|p|RUNNING|1|N/A|N/A|n1|1:00:00|train|n1",
        ).unwrap();
        snapshot.viewer = "me".into();
        snapshot.cpu = Some(crate::slurm_parser::parse_cpu("n1|32/32/0/64|12.4").unwrap());
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        app.key(KeyCode::Char('2'));
        for width in [60, 100, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = buffer
                .content()
                .chunks(width as usize)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect())
                .collect();
            assert!(
                rows.iter().any(|row| row.contains("Allocated CPU 32/64")),
                "{rows:#?}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.contains("Allocated GPU h100 2/4")),
                "{rows:#?}"
            );
        }
    }

    #[test]
    fn instructions_follow_panels_at_all_terminal_sizes() {
        for (width, height) in [(80, 24), (120, 60), (50, 80)] {
            for tab in [0, 1] {
                let snapshot = crate::slurm_parser::parse(
                    "NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p\n",
                    "",
                )
                .unwrap();
                let mut app = App::new(String::new());
                app.set_snapshot(snapshot);
                app.tab = tab;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let rows: Vec<String> = terminal
                    .backend()
                    .buffer()
                    .content()
                    .chunks(width as usize)
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect())
                    .collect();
                let instructions = rows
                    .iter()
                    .position(|row| {
                        row.starts_with("1 Schedule  2 Jobs")
                            || row.starts_with("1 schedule  2 jobs")
                    })
                    .unwrap();
                assert!(
                    rows[instructions - 2].contains('┘'),
                    "{width}x{height}, tab {tab}"
                );
            }
        }
    }

    #[test]
    fn initial_refresh_error_explains_how_to_retry() {
        let mut app = App::new(String::new());
        app.error = Some("squeue failed".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Unable to load Slurm data: squeue failed"));
        assert!(screen.contains("Press r to retry or q to quit."));
        assert!(!screen.contains("Connecting to Slurm"));
    }

    #[test]
    fn traffic_summary_uses_owned_totals() {
        let traffic = Traffic {
            requests: 3,
            received: 2048,
            started: std::time::Instant::now() - std::time::Duration::from_secs(2),
        };
        let summary = traffic.summary(300);
        assert!(summary.contains("SSH: 3 req | RX 2.0 KiB"));
        assert!(summary.contains("poll 300s"));
        assert!(!summary.contains("NaN"));
    }

    #[test]
    fn traffic_indicator_is_visible_before_data_arrives() {
        for width in [60, 80, 120] {
            let mut app = App::new(String::new());
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let row: String = terminal
                .backend()
                .buffer()
                .content()
                .chunks(width as usize)
                .nth(1)
                .unwrap()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                row.contains("SSH: 0 req | RX 0.0 KiB | avg 0 B/s | poll 300s"),
                "{row}"
            );
            assert!(row.ends_with("poll 300s"));
        }
    }

    #[test]
    fn starts_with_first_partition_and_cycles_only_real_partitions() {
        let snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=4 CPUAlloc=0 RealMemory=4096 AllocMem=0 State=IDLE Partitions=zeta,alpha Gres=(null) GresUsed=(null)\n",
            "",
        ).unwrap();
        let mut app = App::new(String::new());
        assert_eq!(app.partitions(), vec![String::new()]);
        app.set_snapshot(snapshot);
        assert_eq!(app.estimate_input().unwrap().1, "alpha");
        app.key(KeyCode::Char('p'));
        assert_eq!(app.estimate_input().unwrap().1, "zeta");
        app.key(KeyCode::Char('p'));
        assert_eq!(app.estimate_input().unwrap().1, "alpha");
    }

    #[test]
    fn missing_partition_is_not_treated_as_all_partitions() {
        let snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=4 CPUAlloc=0 RealMemory=4096 AllocMem=0 State=IDLE Partitions=(null) Gres=(null) GresUsed=(null)\n", ""
        ).unwrap();
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        assert!(app.estimate_input().is_none());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("No partitions reported by Slurm"));
        assert!(!screen.contains("all partitions"));
    }

    #[test]
    fn cpu_load_ratio_handles_missing_zero_and_overload() {
        let mut node = crate::slurm_parser::parse_cpu("n1|4/4/0/8|1.25\n")
            .unwrap()
            .remove(0);
        assert!(cpu_measurement(Some(&node), 10).contains("15.6%"));
        assert!(cpu_measurement(Some(&node), 10).contains("[##--------]"));
        node.load = Some(12.0);
        assert!(cpu_measurement(Some(&node), 10).contains("[##########]"));
        assert!(cpu_measurement(Some(&node), 10).contains("150.0%"));
        node.total = 0;
        assert!(cpu_measurement(Some(&node), 10).contains("unavailable"));
        node.total = 8;
        node.load = None;
        assert!(cpu_measurement(Some(&node), 10).contains("unavailable"));
        assert!(cpu_measurement(None, 10).contains("unavailable"));
    }

    #[test]
    fn running_nodes_can_scroll_to_end_and_back() {
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n01 CPUTot=8 CPUAlloc=4 RealMemory=4096 AllocMem=1000 State=MIXED Partitions=p Gres=(null) GresUsed=(null)\n",
            "42|me|p|RUNNING|20|N/A|N/A|n[01-20]|1:00:00|test-job|n01\n",
        ).unwrap();
        snapshot.viewer = "me".into();
        let cpu = (1..=20)
            .map(|i| format!("n{i:02}|4/4/0/8|1.25\n"))
            .collect::<String>();
        snapshot.cpu = Some(crate::slurm_parser::parse_cpu(&cpu).unwrap());
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        app.key(KeyCode::Char('2'));
        for _ in 0..100 {
            app.key(KeyCode::Down);
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("n20"));
        let at_end = app.running_scroll;
        app.key(KeyCode::Up);
        assert_eq!(app.running_scroll, at_end - 1);
    }

    #[test]
    fn jobs_tab_scrolls_running_and_pending_independently() {
        let mut app = App::new(String::new());
        app.key(KeyCode::Char('2'));
        app.key(KeyCode::Down);
        app.key(KeyCode::Down);
        app.key(KeyCode::Char('j'));
        assert_eq!(app.running_scroll, 2);
        assert_eq!(app.own_scroll, 1);
        app.key(KeyCode::Up);
        app.key(KeyCode::Char('k'));
        assert_eq!(app.running_scroll, 1);
        assert_eq!(app.own_scroll, 0);
    }

    #[test]
    fn switching_account_survives_refresh_and_can_reset() {
        let make_snapshot = || {
            let mut snapshot = crate::slurm_parser::parse(
                "NodeName=n1 CPUTot=4 CPUAlloc=0 RealMemory=4096 AllocMem=0 State=IDLE Partitions=p Gres=(null) GresUsed=(null)\n",
                "1|alice|p|PENDING|1|N/A|N/A|(null)|1:00:00|test-job|Resources\n2|bob|p|PENDING|1|N/A|N/A|(null)|1:00:00|test-job|Priority\n",
            ).unwrap();
            snapshot.viewer = "alice".into();
            snapshot
        };
        let mut app = App::new(String::new());
        app.set_snapshot(make_snapshot());
        app.key(KeyCode::F(2));
        for _ in 0..5 {
            app.key(KeyCode::Backspace);
        }
        for c in "bob".chars() {
            app.key(KeyCode::Char(c));
        }
        app.key(KeyCode::Enter);
        assert_eq!(app.snapshot.as_ref().unwrap().viewer, "bob");
        app.set_snapshot(make_snapshot());
        assert_eq!(app.snapshot.as_ref().unwrap().viewer, "bob");
        let projection =
            Projection::queue(app.snapshot.as_ref().unwrap(), "p", chrono::Local::now());
        assert!(projection.columns.iter().any(|c| c.own_simulated > 0));
        app.key(KeyCode::F(2));
        for _ in 0..3 {
            app.key(KeyCode::Backspace);
        }
        app.key(KeyCode::Enter);
        assert_eq!(app.snapshot.as_ref().unwrap().viewer, "alice");
    }

    #[test]
    fn jobs_tab_shows_node_cpu_load_and_pending_reason() {
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=8 CPUAlloc=4 RealMemory=4096 AllocMem=1000 State=MIXED Partitions=p Gres=(null) GresUsed=(null)\n",
            "42|me|p|RUNNING|1|N/A|N/A|n1|1:00:00|test-job|n1\n43|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|test-job|Priority\n",
        ).unwrap();
        snapshot.viewer = "me".into();
        snapshot.cpu = Some(crate::slurm_parser::parse_cpu("n1|4/4/0/8|1.25\n").unwrap());
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        app.key(KeyCode::Char('2'));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Running · node load"));
        assert!(screen.contains("Allocated 4/8"));
        assert!(screen.contains("Job 42: test-job"));
        assert!(screen.contains("1.25"));
        assert!(screen.contains("LOAD / CPUS"));
        assert!(screen.contains("15.6%"));
        assert!(screen.contains("[###------------------]"));
        assert!(screen.contains("43  p  1 nodes"));
        assert!(screen.contains("Reason: Priority"));
    }

    #[test]
    fn small_personal_allocation_has_a_visible_marker() {
        let column = Column {
            unavailable: 26,
            own_running: 1,
            running: 200,
            ..Column::default()
        };
        let projection = Projection {
            columns: vec![column; TIME_BUCKETS],
            nodes: 251,
            unresolved: 0,
            horizon: 3600,
            latest_end: 0,
            placements: vec![],
            running_bands: vec![],
            pending_bands: vec![],
        };
        let lines = plot(&projection, 5, TIME_BUCKETS);
        let picture: String = lines[..5]
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(picture.contains('◆'));
        assert!(picture.contains('█'));
        assert_eq!(lines[5].spans.len(), 1);
    }

    #[test]
    fn separate_small_proxy_jobs_each_get_a_marker() {
        let now = chrono::Local::now();
        let nodes = (1..=100)
            .map(|i| format!("NodeName=n{i} CPUAlloc=0 State=IDLE Partitions=p\n"))
            .collect::<String>();
        let mut snapshot = crate::slurm_parser::parse(
            &nodes,
            "1|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|one|Resources\n2|me|p|PENDING|20|N/A|N/A|(null)|1:00:00|two|Resources\n",
        ).unwrap();
        snapshot.viewer = "me".into();
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now);
        assert_eq!(projection.placements.len(), 2);
        let lines = plot(&projection, 10, TIME_BUCKETS);
        // The one-node job needs its own fallback; the larger job samples row 1.
        assert_eq!(lines[9].spans[1].content, "◆");
        assert_eq!(lines[8].spans[1].content, "◆");
        assert_ne!(lines[9].spans[1].style.fg, lines[8].spans[1].style.fg);
        assert_eq!(lines[9].spans[1].style.fg, lines[9].spans[2].style.fg);
        assert_eq!(lines[8].spans[1].style.fg, lines[8].spans[2].style.fg);
        assert_eq!(
            lines[..10]
                .iter()
                .filter(|line| line.spans[1].content == "◆")
                .count(),
            2
        );
    }

    #[test]
    fn small_proxy_job_remains_visible_in_a_compressed_timeline() {
        let mut columns = vec![Column::default(); TIME_BUCKETS];
        columns[1] = Column {
            running: 398,
            slurm: 100,
            own_simulated: 1,
            ..Column::default()
        };
        columns[0].running = 550;
        let projection = Projection {
            columns,
            nodes: 576,
            unresolved: 0,
            horizon: 48 * 3600,
            latest_end: 0,
            placements: vec![],
            running_bands: vec![],
            pending_bands: vec![],
        };
        let compressed = chart_columns(&projection, TIME_BUCKETS / 2);
        assert_eq!(compressed[0].0.own_simulated, 1);
        let lines = plot(&projection, 24, TIME_BUCKETS / 2);
        assert!(lines[..24].iter().any(|line| line.spans[1].content == "◆"));
        assert!(!lines[..24].iter().any(|line| line.spans[2].content == "◆"));
    }

    #[test]
    fn narrow_chart_keeps_short_allocations_visible() {
        let mut columns = vec![Column::default(); TIME_BUCKETS];
        columns[1].own_running = 1;
        let projection = Projection {
            columns,
            nodes: 2,
            unresolved: 0,
            horizon: 3600,
            latest_end: 0,
            placements: vec![],
            running_bands: vec![],
            pending_bands: vec![],
        };
        let compressed = chart_columns(&projection, TIME_BUCKETS / 2);
        assert_eq!(compressed[0].0.own_running, 1);
        let expanded = chart_columns(&projection, TIME_BUCKETS * 2);
        assert_eq!(expanded.len(), TIME_BUCKETS * 2);
        assert_eq!(expanded[2].0.own_running, 1);
        assert_eq!(expanded[3].0.own_running, 1);
        assert!(time_axis(48 * 3600, 28).contains("48h"));
        let axis = time_axis(221 * 3600 + 5 * 60, 100);
        assert!(axis.contains("96h"));
        assert!(axis.contains("221h5m"));
    }

    #[test]
    fn job_colors_distinguish_overlapping_jobs_and_remain_stable() {
        let now = chrono::Local::now();
        let nodes = (1..=10).map(|i| format!("NodeName=n{i} CPUTot=8 CPUAlloc=0 RealMemory=4096 AllocMem=0 State=IDLE Partitions=p Gres=(null) GresUsed=(null)\n")).collect::<String>();
        let mut snapshot = crate::slurm_parser::parse(&nodes,
            "1|a|p|RUNNING|3|N/A|N/A|n[1-3]|1:00:00|one|n1\n2|b|p|RUNNING|2|N/A|N/A|n[4-5]|2:00:00|two|n4\n3|a|p|PENDING|3|N/A|N/A|(null)|1:00:00|three|Resources\n4|b|p|PENDING|2|N/A|N/A|(null)|1:00:00|four|Priority\n").unwrap();
        snapshot.jobs[0].end = Some(now + chrono::Duration::hours(1));
        snapshot.jobs[1].end = Some(now + chrono::Duration::hours(2));
        snapshot.jobs[2].start = Some(now + chrono::Duration::hours(1));
        snapshot.jobs[3].start = Some(now + chrono::Duration::hours(1));
        let projection = Projection::build(&snapshot, "p", 48 * 3600, now);
        let first = projection.running_bands[0][0];
        let second = projection.running_bands[0][1];
        assert_eq!(
            (first.start, first.end, second.start, second.end),
            (0, 3, 3, 5)
        );
        assert_ne!(first.color, second.color);
        assert_eq!(projection.running_bands[8][0].color, second.color);
        let running = plot(&projection, 10, TIME_BUCKETS);
        assert_ne!(running[7].spans[1].style.fg, running[6].spans[1].style.fg);
        assert_eq!(running[7].spans[1].content, "█");
        assert_eq!(running[8].spans[9].style.fg, running[6].spans[1].style.fg);
        assert_eq!(running[7].spans[9].content, "▓");
        assert_eq!(running[4].spans[9].content, "▓");
        assert_ne!(running[7].spans[9].style.fg, running[4].spans[9].style.fg);
    }

    #[test]
    fn charts_put_percentage_ticks_and_node_count_on_left() {
        let projection = Projection {
            columns: vec![
                Column {
                    running: 125,
                    simulated: 125,
                    ..Column::default()
                };
                TIME_BUCKETS
            ],
            nodes: 251,
            unresolved: 0,
            horizon: 48 * 3600,
            latest_end: 1,
            placements: vec![],
            running_bands: vec![],
            pending_bands: vec![],
        };
        let lines = chart(Some(&projection), 9, 60);
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let margin = "(251 nodes)".len();
        for tick in ["100%", "50%", "25%"] {
            assert!(rows
                .iter()
                .any(|row| row.starts_with(&format!("{tick:>margin$} │"))));
        }
        let bottom = rows
            .iter()
            .position(|row| row.starts_with(&format!("{:>margin$} └", "0%")))
            .unwrap();
        assert!(rows[bottom + 1].starts_with("(251 nodes)  now"));
    }

    #[test]
    fn personal_markers_cover_scheduled_and_proxy_jobs() {
        let mut columns = vec![Column::default(); TIME_BUCKETS];
        columns[0].own_slurm = 1;
        columns[1].own_simulated = 1;
        let projection = Projection {
            columns,
            nodes: 4,
            unresolved: 0,
            horizon: 3600,
            latest_end: 1,
            placements: vec![],
            running_bands: vec![],
            pending_bands: vec![],
        };
        let top = plot(&projection, 4, TIME_BUCKETS);
        let bottom = &top;
        assert_eq!(top[3].spans[1].content, "◆");
        assert_eq!(bottom[3].spans[2].content, "◆");
        assert_eq!(top[3].spans[1].style.fg, Some(Color::LightGreen));
        assert_eq!(bottom[3].spans[2].style.fg, Some(Color::LightGreen));
    }

    #[test]
    fn combined_plot_distinguishes_running_scheduled_and_proxy_jobs() {
        let mut columns = vec![Column::default(); TIME_BUCKETS];
        columns[0] = Column {
            running: 2,
            slurm: 3,
            simulated: 2,
            ..Column::default()
        };
        let mut running_bands = vec![Vec::new(); TIME_BUCKETS];
        running_bands[0].push(crate::projection::JobBand {
            start: 0,
            end: 2,
            color: 0,
        });
        let mut pending_bands = vec![Vec::new(); TIME_BUCKETS];
        pending_bands[0].push(crate::projection::JobBand {
            start: 0,
            end: 3,
            color: 0,
        });
        pending_bands[0].push(crate::projection::JobBand {
            start: 3,
            end: 5,
            color: 1,
        });
        let projection = Projection {
            columns,
            nodes: 10,
            unresolved: 0,
            horizon: 3600,
            latest_end: 1,
            placements: vec![],
            running_bands,
            pending_bands,
        };
        let top = plot(&projection, 10, TIME_BUCKETS);
        assert_eq!(top[9].spans[1].content, "█");
        assert_eq!(top[7].spans[1].content, "▓");
        assert_eq!(top[7].spans[1].style.fg, Some(Color::Rgb(244, 93, 93)));
        let bottom = &top;
        assert_eq!(bottom[4].spans[1].content, "▒");
        assert_eq!(bottom[4].spans[1].style.fg, Some(Color::Rgb(155, 115, 230)));
        assert_eq!(bottom[2].spans[1].content, " ");
    }

    #[test]
    fn dashboard_shows_capacity_and_own_job_markers() {
        let now = chrono::Local::now();
        let mut snapshot = crate::slurm_parser::parse(
            "NodeName=n1 CPUTot=4 CPUAlloc=4 RealMemory=4096 AllocMem=1000 State=ALLOCATED Partitions=p Gres=(null) GresUsed=(null)\n",
            "42|me|p|RUNNING|1|N/A|N/A|n1|1:00:00|test-job|Resources\n",
        ).unwrap();
        snapshot.viewer = "me".into();
        snapshot.jobs[0].end = Some(now + chrono::Duration::hours(1));
        let mut app = App::new(String::new());
        app.projection = Some(Projection::build(&snapshot, "", 48 * 3600, now));
        app.snapshot = Some(snapshot);
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        let graph_rows = buffer
            .content()
            .chunks(70)
            .filter(|row| row.iter().any(|cell| cell.symbol() == "◆"))
            .count();
        assert!(graph_rows >= 5, "chart needs enough vertical resolution");
        assert!(screen.contains("42  running"));
        assert!(screen.contains("Your schedule"));
        assert!(screen.contains("▒ proxy schedule"));
        assert!(!screen.contains("Fast starts"));
        assert!(screen.contains("Running + scheduled + proxy"));
        assert!(screen.contains("Partition: p"));
        assert!(!screen.contains("z zoom"));
        assert!(screen.contains("your jobs: 1"));
        assert!(screen.contains("1 nodes"));
        assert!(!screen.contains("RRR"));
    }

    #[test]
    fn tabs_share_panel_geometry_across_terminal_sizes() {
        let snapshot =
            crate::slurm_parser::parse("NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p\n", "")
                .unwrap();
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        for (width, height) in [(100, 80), (80, 24), (50, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut corners = Vec::new();
            for tab in ['1', '2'] {
                app.key(KeyCode::Char(tab));
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                corners.push(
                    terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .enumerate()
                        .filter(|(_, cell)| matches!(cell.symbol(), "┌" | "┐" | "└" | "┘"))
                        .map(|(index, cell)| (index, cell.symbol().to_owned()))
                        .collect::<Vec<_>>(),
                );
            }
            assert_eq!(corners[0].len(), 8, "two panels at {width}x{height}");
            assert_eq!(corners[0], corners[1], "panel bounds at {width}x{height}");
        }
    }

    #[test]
    fn tall_terminal_caps_chart_height() {
        let snapshot =
            crate::slurm_parser::parse("NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p\n", "")
                .unwrap();
        let mut app = App::new(String::new());
        app.set_snapshot(snapshot);
        let mut terminal = Terminal::new(TestBackend::new(100, 80)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 4 + 27)].symbol(), "└");
        assert_eq!(buffer[(0, 4 + 28)].symbol(), "█");
        assert_eq!(buffer[(0, 4 + 29)].symbol(), "1");
        assert_eq!(buffer[(76, 4 + 27)].symbol(), "└");
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn app(jobs: &str) -> App {
        let mut snapshot =
            crate::slurm_parser::parse("NodeName=n1 CPUAlloc=0 State=IDLE Partitions=p", jobs)
                .unwrap();
        snapshot.viewer = "me".into();
        let mut app = App::new("test".into());
        app.set_snapshot(snapshot);
        app
    }
    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    #[test]
    fn completing_jobs_remain_in_personal_schedule() {
        let mut app = app("finishing-job|me|p|COMPLETING|1|N/A|N/A|n1|1:00:00|training|None");
        let schedule = screen(&mut app);
        assert!(schedule.contains("finishing-job"));
        assert!(schedule.contains("completing"));
        app.snapshot.as_mut().unwrap().cpu =
            Some(crate::slurm_parser::parse_cpu("n1|1/0/0/1|1").unwrap());
        app.tab = 1;
        assert!(screen(&mut app).contains("Job finishing-job"));
    }
    #[test]
    fn wrapped_pending_rows_can_scroll_to_last_job() {
        let reason = "DependencyNeverSatisfiedWithAnExtremelyLongAccountOrJobDependencyDescription";
        let jobs = (1..=4)
            .map(|n| format!("job{n}|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|job|{reason}\n"))
            .collect::<String>();
        let mut app = app(&jobs);
        app.tab = 1;
        for _ in 0..20 {
            app.key(KeyCode::Char('j'));
            screen(&mut app);
        }
        assert!(screen(&mut app).contains("job4"));
    }
    #[test]
    fn one_up_moves_after_scrolling_past_end() {
        let jobs = (1..=10)
            .map(|n| format!("job{n}|me|p|PENDING|1|N/A|N/A|(null)|1:00:00|job|Priority\n"))
            .collect::<String>();
        let mut app = app(&jobs);
        app.tab = 1;
        for _ in 0..100 {
            app.key(KeyCode::Char('j'));
            screen(&mut app);
        }
        let bottom = screen(&mut app);
        app.key(KeyCode::Char('k'));
        assert_ne!(screen(&mut app), bottom);
    }
}
