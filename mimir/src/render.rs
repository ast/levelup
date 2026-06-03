//! Turn the blocks' [`Segment`]s into swaybar protocol [`Block`]s, applying the
//! cockpit palette: the icon is painted as a cyan "label" and the value in its
//! role/level colour (green data, white time, amber/red alerts), via Pango.

use crate::blocks::{Level, Role, Segment};
use crate::config::{Colors, Config, Markup};
use crate::protocol::{self, Block};

/// Convert one render pass's segments into protocol blocks.
pub fn to_protocol(segments: Vec<Segment>, cfg: &Config) -> Vec<Block> {
    segments
        .into_iter()
        .map(|seg| segment_to_block(seg, cfg))
        .collect()
}

fn segment_to_block(seg: Segment, cfg: &Config) -> Block {
    let value_color = value_color(seg.role, seg.level, &cfg.colors);

    let (full_text, color, markup) = match cfg.markup {
        Markup::Pango => {
            // Two-tone: cyan icon ("label") + role-coloured value. Colours are
            // embedded in the markup, so the block-level `color` is unset.
            let mut s = String::new();
            if let Some(icon) = &seg.icon {
                s.push_str(&span(&cfg.colors.label, icon));
                s.push(' ');
            }
            s.push_str(&span(value_color, &seg.text));
            (s, None, Some("pango".to_string()))
        }
        Markup::None => {
            // No markup → can't two-tone; colour the whole block as the value.
            let text = match &seg.icon {
                Some(icon) => format!("{icon} {}", seg.text),
                None => seg.text.clone(),
            };
            let color = (!value_color.is_empty()).then(|| value_color.to_string());
            (text, color, None)
        }
    };

    let short_text = seg.short.map(|s| match cfg.markup {
        Markup::Pango => protocol::pango_escape(&s),
        Markup::None => s,
    });
    Block {
        full_text,
        short_text,
        color,
        name: seg.name,
        instance: seg.instance,
        markup,
        // Deliberately never set `urgent`: swaybar renders urgent blocks with a
        // border, which widens the block and shifts the bar. Critical is shown
        // by colour alone, which is geometry-neutral.
        urgent: false,
    }
}

/// The colour for a segment's *value*: time → white; data → green / amber /
/// red by level.
fn value_color(role: Role, level: Level, colors: &Colors) -> &str {
    match role {
        Role::Time => &colors.time,
        Role::Data => match level {
            Level::Normal => &colors.data,
            Level::Warn => &colors.warn,
            Level::Critical => &colors.critical,
        },
    }
}

/// Wrap `text` (Pango-escaped) in a coloured span, or leave it bare if `color`
/// is empty (use the bar's default).
fn span(color: &str, text: &str) -> String {
    let esc = protocol::pango_escape(text);
    if color.is_empty() {
        esc
    } else {
        format!("<span color='{color}'>{esc}</span>")
    }
}

/// Render segments as a single plain-text line (icon + value, no colour) for
/// `mimir once` — a human-readable snapshot for debugging.
pub fn to_plain(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| match &s.icon {
            Some(icon) => format!("{icon} {}", s.text),
            None => s.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("  |  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Segment;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn icon_and_value_get_distinct_colours() {
        let segs = vec![Segment::new("mem", "19G").with_icon("M")];
        let blocks = to_protocol(segs, &cfg());
        // Cyan label span for the icon, green data span for the value.
        assert_eq!(
            blocks[0].full_text,
            "<span color='#1ec8e6'>M</span> <span color='#34d63a'>19G</span>"
        );
        assert_eq!(blocks[0].markup.as_deref(), Some("pango"));
        assert!(blocks[0].color.is_none());
    }

    #[test]
    fn critical_value_is_red_not_urgent() {
        let segs = vec![
            Segment::new("cpu", "99%")
                .with_icon("C")
                .with_level(Level::Critical),
        ];
        let blocks = to_protocol(segs, &cfg());
        assert!(!blocks[0].urgent);
        assert!(
            blocks[0]
                .full_text
                .contains("<span color='#ff3b30'>99%</span>")
        );
    }

    #[test]
    fn time_role_is_white() {
        let segs = vec![Segment::new("clock", "12:24").with_role(Role::Time)];
        let blocks = to_protocol(segs, &cfg());
        assert_eq!(blocks[0].full_text, "<span color='#e8e8e8'>12:24</span>");
    }

    #[test]
    fn pango_specials_escaped_inside_spans() {
        let segs = vec![Segment::new("net", "a & b").with_icon("N")];
        let blocks = to_protocol(segs, &cfg());
        assert!(blocks[0].full_text.contains("a &amp; b"));
    }

    #[test]
    fn plain_join_includes_icons() {
        let segs = vec![
            Segment::new("a", "one").with_icon("A"),
            Segment::new("b", "two"),
        ];
        assert_eq!(to_plain(&segs), "A one  |  two");
    }
}
