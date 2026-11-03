//! Passive shortcut hints arranged in responsive columns above the composer.

use super::selection_popup_common::menu_surface_inset;
use super::selection_popup_common::menu_surface_padding_height;
use super::selection_popup_common::render_menu_surface;
use crate::line_truncation::line_width;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;
use crate::style::accent_color;
use crate::style::secondary_text_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

const COLUMN_GAP: usize = 2;
const MIN_COLUMN_WIDTH: usize = 32;
const COMMAND_SEPARATOR: &str = "   ";
const SINGLE_COLUMN_ENTRY_LIMIT: usize = 5;

pub(crate) struct ShortcutHud {
    title: String,
    entries: Vec<Line<'static>>,
    commands: Vec<Line<'static>>,
}

impl ShortcutHud {
    pub(crate) fn new(
        title: impl Into<String>,
        entries: Vec<Line<'static>>,
        commands: Vec<Line<'static>>,
    ) -> Self {
        Self {
            title: title.into(),
            entries,
            commands,
        }
    }

    pub(crate) fn item(key: impl Into<String>, label: Span<'static>) -> Line<'static> {
        vec![
            key.into().fg(accent_color()),
            " · ".set_style(secondary_text_style()),
            label,
        ]
        .into()
    }

    fn body_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![self.title.clone().bold().into(), Line::default()];
        let max_columns = if self.entries.len() > SINGLE_COLUMN_ENTRY_LIMIT {
            2
        } else {
            1
        };
        lines.extend(grid_lines(&self.entries, width, max_columns));
        lines
    }

    fn command_lines(&self, width: u16) -> Vec<Line<'static>> {
        crate::footer_hint::wrap_hint_rows(
            self.commands.iter().cloned(),
            width,
            COMMAND_SEPARATOR.len(),
            Line::width,
        )
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (index, command) in row.into_iter().enumerate() {
                if index > 0 {
                    spans.push(COMMAND_SEPARATOR.into());
                }
                spans.extend(command.spans);
            }
            spans.into()
        })
        .collect()
    }
}

impl Renderable for ShortcutHud {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let content = render_menu_surface(area, buf);
        let commands = self.command_lines(content.width);
        let command_height = u16::try_from(commands.len())
            .unwrap_or(u16::MAX)
            .min(content.height);
        let gap = content
            .height
            .saturating_sub(command_height)
            .min(/*other*/ 1);
        let body = Rect {
            height: content.height.saturating_sub(command_height + gap),
            ..content
        };
        let command_area = Rect {
            y: content.bottom().saturating_sub(command_height),
            height: command_height,
            ..content
        };
        render_clipped(self.body_lines(content.width), body, buf);
        Paragraph::new(commands).render(command_area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let content_width = menu_surface_inset(Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            width,
            /*height*/ u16::MAX,
        ))
        .width;
        let body_height = self.body_lines(content_width).len();
        let command_height = self.command_lines(content_width).len();
        u16::try_from(body_height + 1 + command_height)
            .unwrap_or(u16::MAX)
            .saturating_add(menu_surface_padding_height())
    }
}

fn grid_lines(items: &[Line<'static>], width: u16, max_columns: usize) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let columns = ((width + COLUMN_GAP) / (MIN_COLUMN_WIDTH + COLUMN_GAP))
        .clamp(/*min*/ 1, max_columns)
        .min(items.len().max(/*other*/ 1));
    let rows = items.len().div_ceil(columns);
    let cell_width = width.saturating_sub(COLUMN_GAP * (columns - 1)) / columns;
    (0..rows)
        .map(|row| {
            let mut line = Line::default();
            for column in 0..columns {
                let item = items.get(column * rows + row).cloned().unwrap_or_default();
                let item = truncate_line_with_ellipsis_if_overflow(item, cell_width);
                let padding = cell_width.saturating_sub(line_width(&item)) + COLUMN_GAP;
                line.extend(item.spans);
                if column + 1 < columns {
                    line.push_span(" ".repeat(padding));
                }
            }
            line
        })
        .collect()
}

fn render_clipped(mut lines: Vec<Line<'static>>, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let height = usize::from(area.height);
    if lines.len() > height {
        lines.truncate(height);
        lines[height - 1] = "… resize to see all".dim().into();
    }
    Paragraph::new(lines).render(area, buf);
}
