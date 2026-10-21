//! Feature status widget.
//!
//! One line per key feature, each saying what the feature is actually doing
//! this session rather than what the configuration asked for: the GPU the
//! encoder selected, whether night lighting found the X-Plane installation,
//! and which X-World Pro continents XEarthLayer is yielding overlays to.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use xearthlayer::night::NightLighting;
use xearthlayer::system::GpuAdapter;
use xearthlayer::xworld_pro::Detection;

/// Texture encoding: the GPU that was selected, or the CPU encoder in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderStatus {
    Gpu(GpuAdapter),
    /// The `texture.compressor` value (`ispc` or `software`).
    Cpu(String),
}

/// X-World Pro support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XworldProStatus {
    /// `packages.xworld_pro` is off, so nothing was looked for.
    SupportOff,
    /// Support is on but `scenery_packs.ini` names no X-World Pro entry.
    NotDetected,
    /// Installed, but no layer is enabled with its folder present.
    NoActiveContinents,
    /// The continents with an active layer, in X-World Pro's own names.
    Active(Vec<&'static str>),
}

impl XworldProStatus {
    /// From `packages.xworld_pro` and the detection preflight contributed,
    /// which it does only while that setting is on.
    pub fn from_detection(enabled: bool, detection: Option<&Detection>) -> Self {
        let Some(detection) = detection.filter(|_| enabled) else {
            return if enabled {
                Self::NotDetected
            } else {
                Self::SupportOff
            };
        };
        if !detection.is_present() {
            return Self::NotDetected;
        }
        let continents: Vec<&'static str> = detection
            .active_continents()
            .into_iter()
            .map(|c| c.label())
            .collect();
        if continents.is_empty() {
            Self::NoActiveContinents
        } else {
            Self::Active(continents)
        }
    }
}

/// Everything the status pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureStatus {
    pub encoder: EncoderStatus,
    pub night: NightLighting,
    pub xworld_pro: XworldProStatus,
}

/// Widget displaying the feature status lines.
pub struct FeatureStatusWidget<'a> {
    status: &'a FeatureStatus,
    frame_rate: Option<u16>,
}

impl<'a> FeatureStatusWidget<'a> {
    pub fn new(status: &'a FeatureStatus) -> Self {
        Self {
            status,
            frame_rate: None,
        }
    }

    /// X-Plane's current frame rate, or `None` while X-Plane is not
    /// connected or has not reported one. Live, unlike the feature states.
    pub fn with_frame_rate(mut self, frame_rate: Option<u16>) -> Self {
        self.frame_rate = frame_rate;
        self
    }

    fn frame_rate_line(&self) -> Line<'static> {
        match self.frame_rate {
            Some(fps) => line(Mark::On, format!("X-Plane frame rate: {fps} fps")),
            None => line(Mark::Off, "X-Plane frame rate: not connected".to_string()),
        }
    }

    fn encoder_line(&self) -> Line<'static> {
        match &self.status.encoder {
            EncoderStatus::Gpu(adapter) => line(
                Mark::On,
                format!(
                    "GPU acceleration, {} ({})",
                    adapter.name,
                    adapter.kind.display().to_lowercase()
                ),
            ),
            EncoderStatus::Cpu(compressor) => line(
                Mark::Off,
                format!("GPU acceleration off, encoding on CPU ({compressor})"),
            ),
        }
    }

    fn night_line(&self) -> Line<'static> {
        match self.status.night {
            NightLighting::Lit => line(Mark::On, "Night lighting".to_string()),
            NightLighting::Dark => line(
                Mark::Warn,
                "Night lighting on, but X-Plane was not found: lit textures stay dark".to_string(),
            ),
            NightLighting::Off => line(Mark::Off, "Night lighting off".to_string()),
        }
    }

    fn xworld_pro_line(&self) -> Line<'static> {
        match &self.status.xworld_pro {
            XworldProStatus::Active(continents) => line(
                Mark::On,
                format!("X-World Pro detected: {}", continents.join(", ")),
            ),
            XworldProStatus::NoActiveContinents => line(
                Mark::Off,
                "X-World Pro installed, no layers enabled".to_string(),
            ),
            XworldProStatus::NotDetected => line(Mark::Off, "X-World Pro not detected".to_string()),
            XworldProStatus::SupportOff => line(Mark::Off, "X-World Pro support off".to_string()),
        }
    }
}

/// The marker in front of each line: working (green check, green text), not
/// set or not present (grey cross, grey text), or asked for but not working
/// (amber).
#[derive(Clone, Copy)]
enum Mark {
    On,
    Off,
    Warn,
}

fn line(mark: Mark, text: String) -> Line<'static> {
    let (glyph, mark_color, text_color) = match mark {
        Mark::On => ("✓", Color::Green, Color::Green),
        Mark::Off => ("✗", Color::DarkGray, Color::DarkGray),
        Mark::Warn => ("!", Color::Yellow, Color::Yellow),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph, Style::default().fg(mark_color)),
        Span::raw(" "),
        Span::styled(text, Style::default().fg(text_color)),
    ])
}

impl Widget for FeatureStatusWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = vec![
            self.frame_rate_line(),
            self.encoder_line(),
            self.night_line(),
            self.xworld_pro_line(),
        ];
        Paragraph::new(text).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use xearthlayer::system::GpuKind;
    use xearthlayer::xworld_pro::{Continent, Detection, PackEntry, XwpLayer};

    fn radeon() -> GpuAdapter {
        GpuAdapter {
            name: "AMD Radeon 780M".to_string(),
            kind: GpuKind::Integrated,
            backend: "vulkan".to_string(),
        }
    }

    fn all_on() -> FeatureStatus {
        FeatureStatus {
            encoder: EncoderStatus::Gpu(radeon()),
            night: NightLighting::Lit,
            xworld_pro: XworldProStatus::Active(vec!["America", "Europe"]),
        }
    }

    fn render_with(widget: FeatureStatusWidget<'_>) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, 100, 4);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let rows = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (buf, rows)
    }

    /// Rendered with X-Plane connected at 45 fps.
    fn render(status: &FeatureStatus) -> (Buffer, Vec<String>) {
        render_with(FeatureStatusWidget::new(status).with_frame_rate(Some(45)))
    }

    /// Colour of the marker cell at the start of `row`.
    fn marker(buf: &Buffer, row: u16) -> Color {
        buf[(2, row)].fg
    }

    /// Colour of the first cell of the text after the marker on `row`.
    fn text(buf: &Buffer, row: u16) -> Color {
        buf[(4, row)].fg
    }

    fn layer(continent: Continent, enabled: bool, resolved: bool) -> XwpLayer {
        XwpLayer {
            entry: PackEntry {
                line: 1,
                enabled,
                path: format!(
                    "Custom Scenery/X-World-Pro_{}_10-network/",
                    continent.label()
                ),
            },
            continent,
            layer: "10-network".to_string(),
            dir: std::path::PathBuf::from("/xplane/Custom Scenery"),
            resolved,
        }
    }

    #[test]
    fn x_world_pro_status_follows_the_setting_and_the_detection() {
        assert_eq!(
            XworldProStatus::from_detection(false, None),
            XworldProStatus::SupportOff
        );
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&Detection::default())),
            XworldProStatus::NotDetected
        );

        let disabled = Detection {
            layers: vec![layer(Continent::Europe, false, true)],
            ..Detection::default()
        };
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&disabled)),
            XworldProStatus::NoActiveContinents
        );

        let active = Detection {
            layers: vec![
                layer(Continent::Europe, true, true),
                layer(Continent::America, true, true),
                layer(Continent::Asia, true, false),
            ],
            ..Detection::default()
        };
        assert_eq!(
            XworldProStatus::from_detection(true, Some(&active)),
            XworldProStatus::Active(vec!["America", "Europe"]),
            "only enabled layers whose folder exists count"
        );
    }

    #[test]
    fn everything_on_reads_as_green_checked_lines() {
        let (buf, rows) = render(&all_on());
        assert_eq!(
            rows,
            vec![
                "  ✓ X-Plane frame rate: 45 fps",
                "  ✓ GPU acceleration, AMD Radeon 780M (integrated)",
                "  ✓ Night lighting",
                "  ✓ X-World Pro detected: America, Europe",
            ]
        );
        for row in 0..4 {
            assert_eq!(marker(&buf, row), Color::Green, "marker on row {row}");
            assert_eq!(text(&buf, row), Color::Green, "text on row {row}");
        }
    }

    #[test]
    fn frame_rate_reads_not_connected_without_x_plane() {
        let status = all_on();
        let (buf, rows) = render_with(FeatureStatusWidget::new(&status).with_frame_rate(None));
        assert_eq!(rows[0], "  ✗ X-Plane frame rate: not connected");
        assert_eq!(marker(&buf, 0), Color::DarkGray);
        assert_eq!(text(&buf, 0), Color::DarkGray);
    }

    #[test]
    fn a_cpu_encoder_reads_as_gpu_acceleration_off() {
        let status = FeatureStatus {
            encoder: EncoderStatus::Cpu("ispc".to_string()),
            ..all_on()
        };
        let (buf, rows) = render(&status);
        assert_eq!(rows[1], "  ✗ GPU acceleration off, encoding on CPU (ispc)");
        assert_eq!(marker(&buf, 1), Color::DarkGray);
        assert_eq!(text(&buf, 1), Color::DarkGray);
    }

    #[test]
    fn night_lighting_that_found_no_installation_is_a_warning() {
        let status = FeatureStatus {
            night: NightLighting::Dark,
            ..all_on()
        };
        let (buf, rows) = render(&status);
        assert_eq!(
            rows[2],
            "  ! Night lighting on, but X-Plane was not found: lit textures stay dark"
        );
        assert_eq!(marker(&buf, 2), Color::Yellow);
    }

    #[test]
    fn night_lighting_off_is_a_grey_cross() {
        let status = FeatureStatus {
            night: NightLighting::Off,
            ..all_on()
        };
        let (buf, rows) = render(&status);
        assert_eq!(rows[2], "  ✗ Night lighting off");
        assert_eq!(marker(&buf, 2), Color::DarkGray);
        assert_eq!(text(&buf, 2), Color::DarkGray);
    }

    #[test]
    fn x_world_pro_states_without_active_continents_are_grey_crosses() {
        for (xworld_pro, expected) in [
            (XworldProStatus::SupportOff, "  ✗ X-World Pro support off"),
            (XworldProStatus::NotDetected, "  ✗ X-World Pro not detected"),
            (
                XworldProStatus::NoActiveContinents,
                "  ✗ X-World Pro installed, no layers enabled",
            ),
        ] {
            let status = FeatureStatus {
                xworld_pro,
                ..all_on()
            };
            let (buf, rows) = render(&status);
            assert_eq!(rows[3], expected);
            assert_eq!(marker(&buf, 3), Color::DarkGray);
            assert_eq!(text(&buf, 3), Color::DarkGray);
        }
    }
}
