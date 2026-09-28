//! Display units and labels; no process collection or Herdr I/O.

use herdr_resource_monitor::app::PaneSample;
use ratatui::style::{Color, Stylize};
use ratatui::text::Line;

pub(super) fn decimal(value: f64) -> String {
    format!("{value:.2}")
}

pub(super) fn percent(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or_else(|| "—".into(), |value| format!("{}%", decimal(value)))
}

pub(super) fn memory(value: Option<u64>) -> String {
    value.map_or_else(
        || "—".into(),
        |bytes| {
            let (unit, divisor) = if bytes >= 1_000_000_000 {
                ("GB", 1_000_000_000.0)
            } else {
                ("MB", 1_000_000.0)
            };
            format!("{} {unit}", decimal(bytes as f64 / divisor))
        },
    )
}

pub(super) fn usage_color(value: Option<f64>) -> Color {
    match value {
        Some(value) if value.is_finite() && value >= 80.0 => Color::Red,
        Some(value) if value.is_finite() && value >= 50.0 => Color::Yellow,
        Some(value) if value.is_finite() && value >= 0.0 => Color::Green,
        _ => Color::DarkGray,
    }
}

pub(super) fn bar(value: Option<f64>, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let filled = value.filter(|value| value.is_finite()).map_or(0, |value| {
        (value.clamp(0.0, 100.0) / 100.0 * width as f64).round() as usize
    });
    Line::from(format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width - filled)
    ))
    .fg(usage_color(value))
}

// Ratatui supplies display-cell widths, so CJK and emoji titles do not shift
// numeric columns. Keep the original text available through horizontal scroll
// in focused mode and the technical view.
pub(super) fn text_width(text: &str) -> usize {
    Line::from(text).width()
}

pub(super) fn short_label(value: &str, width: usize) -> String {
    if text_width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    for ch in value.chars() {
        let next = format!("{result}{ch}");
        if text_width(&next) >= width {
            break;
        }
        result.push(ch);
    }
    result.push('…');
    result
}

pub(super) fn column(text: &str, width: usize, right: bool) -> String {
    let text = short_label(text, width);
    let padding = " ".repeat(width.saturating_sub(text_width(&text)));
    if right {
        format!("{padding}{text}")
    } else {
        format!("{text}{padding}")
    }
}

pub(super) fn value_lines(key: &str, value: &str, width: u16) -> Vec<Line<'static>> {
    let used = text_width(key) + text_width(value);
    if used < usize::from(width) {
        vec![Line::from(format!(
            "{key}{}{value}",
            " ".repeat(usize::from(width) - used)
        ))]
    } else {
        vec![Line::from(key.to_owned()), Line::from(value.to_owned())]
    }
}

pub(super) fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .into()
}

pub(super) fn terminal_name(pane: &PaneSample) -> String {
    let record = &pane.target.pane;
    record
        .label
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            ["title", "terminal_title_stripped", "terminal_title"]
                .into_iter()
                .find_map(|key| {
                    record
                        .extra
                        .get(key)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                })
        })
        .or_else(|| {
            pane.target
                .agent
                .as_ref()
                .and_then(|agent| agent.info.as_ref())
                .and_then(|info| {
                    ["title", "terminal_title_stripped", "terminal_title"]
                        .into_iter()
                        .find_map(|key| {
                            info.extra
                                .get(key)
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.trim().is_empty())
                        })
                })
        })
        .map(clean)
        .unwrap_or_else(|| format!("Terminal {}", pane.target.pane_id))
}

pub(super) fn agent_name(pane: &PaneSample) -> String {
    pane.target.agent.as_ref().map_or_else(
        || "Shell".into(),
        |agent| {
            [
                agent.name.as_deref(),
                agent.display_agent.as_deref(),
                agent.agent.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .map(clean)
            .unwrap_or_else(|| "Agent".into())
        },
    )
}

pub(super) fn agent_state(pane: &PaneSample) -> &'static str {
    let state = pane
        .target
        .agent
        .as_ref()
        .map_or(pane.target.pane.agent_status.as_str(), |agent| {
            agent.agent_status.as_str()
        });
    match state {
        "working" => "Working",
        "idle" => "Waiting",
        "blocked" => "Needs input",
        "done" => "Done",
        _ => "Unknown",
    }
}

pub(super) fn folder(pane: &PaneSample) -> Option<String> {
    let info = pane
        .target
        .agent
        .as_ref()
        .and_then(|agent| agent.info.as_ref());
    let path = pane
        .target
        .pane
        .foreground_cwd
        .as_deref()
        .or_else(|| info.and_then(|info| info.foreground_cwd.as_deref()))
        .or_else(|| {
            pane.target
                .process
                .foreground_processes
                .iter()
                .find_map(|p| p.cwd.as_deref())
        })
        .or(pane.target.pane.cwd.as_deref())
        .or_else(|| info.and_then(|info| info.cwd.as_deref()))?;
    let display = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .and_then(|home| {
            std::path::Path::new(path)
                .strip_prefix(home)
                .ok()
                .map(|relative| {
                    if relative.as_os_str().is_empty() {
                        "~".into()
                    } else {
                        format!("~/{}", relative.display())
                    }
                })
        })
        .unwrap_or_else(|| path.into());
    Some(clean(&display))
}

// These are optional display aliases for explicitly named metadata. Herdr's
// token maps are open-ended: never infer a model from an agent name/title, or
// treat test-only fixture_model keys as an API contract. All other metadata
// remains available in the technical view.
pub(super) fn metadata<'a>(pane: &'a PaneSample, keys: &[&str]) -> Option<&'a str> {
    let agent = pane.target.agent.as_ref()?;
    keys.iter().find_map(|key| {
        agent
            .tokens
            .get(*key)
            .or_else(|| agent.state_labels.get(*key))
            .map(String::as_str)
            .or_else(|| {
                agent
                    .info
                    .as_ref()
                    .and_then(|info| info.extra.get(*key))
                    .and_then(|v| v.as_str())
            })
            .filter(|value| !value.trim().is_empty())
    })
}

pub(super) fn effort(value: &str) -> String {
    match value {
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        _ => clean(value),
    }
}
