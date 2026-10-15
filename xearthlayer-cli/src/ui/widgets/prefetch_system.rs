//! Prefetch system status widget.
//!
//! Three lines: the prefetcher's state with its flight phase and cycle size,
//! the prefetch lane's load and backlog, and the FUSE generation latency of
//! the last sample (coloured against the generation timeout).

use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::metrics::LatencySummary;
use xearthlayer::prefetch::{DetailedPrefetchStats, FlightPhase, PrefetchState};

const LABEL_COLOR: Color = Color::DarkGray;

/// How full the prefetch lane is: jobs running against its group size, plus
/// the jobs waiting for admission (a full group or the demand pause).
#[derive(Debug, Clone, Copy)]
pub struct LaneView {
    pub running: usize,
    pub group: usize,
    pub queued: usize,
}

/// Widget displaying prefetch system status.
pub struct PrefetchSystemWidget<'a> {
    state: &'a PrefetchState,
    phase: FlightPhase,
    stats: Option<&'a DetailedPrefetchStats>,
    lane: LaneView,
    latency: Option<&'a LatencySummary>,
    generation_timeout: Duration,
    timeouts: usize,
}

impl<'a> PrefetchSystemWidget<'a> {
    pub fn new(
        state: &'a PrefetchState,
        phase: FlightPhase,
        stats: Option<&'a DetailedPrefetchStats>,
        lane: LaneView,
        latency: Option<&'a LatencySummary>,
        generation_timeout: Duration,
    ) -> Self {
        Self {
            state,
            phase,
            stats,
            lane,
            latency,
            generation_timeout,
            timeouts: 0,
        }
    }

    /// FUSE waits that hit the generation timeout in the sample. Shown only
    /// when non-zero: the no-harm bar counts them.
    pub fn with_timeouts(mut self, timeouts: usize) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn state_color(&self) -> Color {
        match self.state {
            PrefetchState::Disabled | PrefetchState::Idle => Color::DarkGray,
            PrefetchState::PausedForDemand => Color::Magenta,
            PrefetchState::HoldingTakeoff | PrefetchState::DeferredLoad => Color::Yellow,
            PrefetchState::Ramping { .. } => Color::Cyan,
            PrefetchState::Active => Color::Green,
        }
    }

    /// Red once a wait outlasts the timeout, amber when p95 or max is past
    /// half of it, green otherwise. The summary includes failed and timed-out
    /// waits, so red is reachable.
    fn latency_color(&self, summary: &LatencySummary) -> Color {
        let timeout_us = self.generation_timeout.as_micros();
        if u128::from(summary.max_us) > timeout_us {
            Color::Red
        } else if u128::from(summary.p95_us) * 2 > timeout_us
            || u128::from(summary.max_us) * 2 > timeout_us
        {
            Color::Yellow
        } else {
            Color::Green
        }
    }

    fn state_line(&self) -> Line<'static> {
        let color = self.state_color();
        let per_cycle = self.stats.map_or(0, |s| s.tiles_submitted_last_cycle);
        Line::from(vec![
            Span::styled("   Prefetch: ", Style::default().fg(LABEL_COLOR)),
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(self.state.label(), Style::default().fg(color)),
            Span::styled(
                format!(" │ {} │ {per_cycle}/cyc", self.phase),
                Style::default().fg(LABEL_COLOR),
            ),
        ])
    }

    fn lane_line(&self) -> Line<'static> {
        Line::from(vec![
            Span::styled("             ", Style::default().fg(LABEL_COLOR)),
            Span::styled(
                format!(
                    "lane {}/{} · {} queued",
                    self.lane.running,
                    self.lane.group,
                    group_thousands(self.lane.queued)
                ),
                Style::default().fg(Color::White),
            ),
        ])
    }

    fn latency_line(&self) -> Line<'static> {
        let label = Span::styled("   Latency:  ", Style::default().fg(LABEL_COLOR));
        let Some(summary) = self.latency else {
            return Line::from(vec![
                label,
                Span::styled(
                    "no FUSE generations this sample",
                    Style::default().fg(LABEL_COLOR),
                ),
            ]);
        };
        let mut text = format!(
            "p50 {} │ p95 {} │ max {}",
            format_latency(summary.median_us),
            format_latency(summary.p95_us),
            format_latency(summary.max_us)
        );
        if self.timeouts > 0 {
            let noun = if self.timeouts == 1 {
                "timeout"
            } else {
                "timeouts"
            };
            text.push_str(&format!(" │ {} {noun}", self.timeouts));
        }
        Line::from(vec![
            label,
            Span::styled(text, Style::default().fg(self.latency_color(summary))),
        ])
    }
}

impl Widget for PrefetchSystemWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = vec![self.state_line(), self.lane_line(), self.latency_line()];
        Paragraph::new(text).render(area, buf);
    }
}

/// Microseconds as `180ms` or `1.4s`; sub-millisecond waits as `340us`.
fn format_latency(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{}ms", us / 1_000)
    } else {
        format!("{us}us")
    }
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use xearthlayer::prefetch::FlightPhase;

    const LANE: LaneView = LaneView {
        running: 28,
        group: 30,
        queued: 1_904,
    };

    fn render(widget: PrefetchSystemWidget<'_>) -> (Buffer, String) {
        let area = Rect::new(0, 0, 100, 3);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        (buf, out)
    }

    fn summary(median_ms: u64, p95_ms: u64, max_ms: u64) -> LatencySummary {
        LatencySummary {
            median_us: median_ms * 1_000,
            p95_us: p95_ms * 1_000,
            max_us: max_ms * 1_000,
            count: 40,
        }
    }

    fn widget<'a>(
        state: &'a PrefetchState,
        latency: Option<&'a LatencySummary>,
    ) -> PrefetchSystemWidget<'a> {
        PrefetchSystemWidget::new(
            state,
            FlightPhase::Cruise,
            None,
            LANE,
            latency,
            Duration::from_secs(10),
        )
    }

    /// Foreground colour of the first cell of `needle` on the latency row.
    fn latency_fg(buf: &Buffer, out: &str, needle: &str) -> Color {
        let row = out
            .lines()
            .position(|l| l.contains("Latency:"))
            .expect("latency row");
        let col = out.lines().nth(row).unwrap().find(needle).expect("needle");
        let col = out.lines().nth(row).unwrap()[..col].chars().count();
        buf[(col as u16, row as u16)].fg
    }

    #[test]
    fn renders_the_state_label() {
        let state = PrefetchState::PausedForDemand;
        let (_, out) = render(widget(&state, None));
        assert!(out.contains("Paused (demand)"), "got:\n{out}");
    }

    #[test]
    fn renders_the_latency_summary() {
        let state = PrefetchState::Active;
        let s = summary(180, 1_400, 3_900);
        let (_, out) = render(widget(&state, Some(&s)));
        assert!(
            out.contains("p50 180ms │ p95 1.4s │ max 3.9s"),
            "got:\n{out}"
        );
        assert!(!out.contains("timeout"), "absent when zero, got:\n{out}");
    }

    #[test]
    fn renders_timeouts_when_nonzero() {
        let state = PrefetchState::Active;
        let s = summary(180, 1_400, 3_900);
        let (_, out) = render(widget(&state, Some(&s)).with_timeouts(2));
        assert!(out.contains("max 3.9s │ 2 timeouts"), "got:\n{out}");
    }

    #[test]
    fn latency_is_green_amber_or_red_against_the_timeout() {
        let state = PrefetchState::Active;

        let calm = summary(100, 800, 2_000);
        let (buf, out) = render(widget(&state, Some(&calm)));
        assert_eq!(latency_fg(&buf, &out, "p50"), Color::Green);

        let slow = summary(100, 6_000, 9_000);
        let (buf, out) = render(widget(&state, Some(&slow)));
        assert_eq!(latency_fg(&buf, &out, "p50"), Color::Yellow);

        let long_tail = summary(100, 800, 6_000);
        let (buf, out) = render(widget(&state, Some(&long_tail)));
        assert_eq!(latency_fg(&buf, &out, "p50"), Color::Yellow);

        let timed_out = summary(100, 6_000, 10_500);
        let (buf, out) = render(widget(&state, Some(&timed_out)));
        assert_eq!(latency_fg(&buf, &out, "p50"), Color::Red);
    }

    #[test]
    fn renders_a_placeholder_without_a_sample() {
        let state = PrefetchState::Idle;
        let (_, out) = render(widget(&state, None));
        assert!(
            out.contains("Latency:  no FUSE generations this sample"),
            "got:\n{out}"
        );
    }

    #[test]
    fn renders_the_lane_view() {
        let state = PrefetchState::Active;
        let (_, out) = render(widget(&state, None));
        assert!(out.contains("lane 28/30 · 1,904 queued"), "got:\n{out}");
    }
}
