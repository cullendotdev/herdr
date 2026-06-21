use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use std::collections::HashSet;

use super::scrollbar::{render_pane_scrollbar, should_show_scrollbar};
use super::widgets::panel_contrast_fg;
use crate::app::state::Palette;
use crate::app::{AppState, Mode};
use crate::config::{PaneBorderMode, PaneBorderStyle};
use crate::layout::{PaneInfo, SplitBorder};
use crate::terminal::{TerminalRuntime, TerminalRuntimeRegistry};

pub(crate) fn pane_is_scrolled_back(rt: &TerminalRuntime) -> bool {
    rt.scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0)
}

fn truncate_label(text: &str, max_width: usize) -> String {
    let len = text.chars().count();
    if len <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let prefix: String = text.chars().take(max_width.saturating_sub(1)).collect();
    format!("{prefix}…")
}

fn pane_border_title(label: &str, pane_width: u16) -> Option<String> {
    let label = label.trim();
    if label.is_empty() || pane_width <= 4 {
        return None;
    }
    let max_label_width = pane_width.saturating_sub(4) as usize;
    Some(format!(" {} ", truncate_label(label, max_label_width)))
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

fn pane_inner_rect(area: Rect, framed: bool) -> Rect {
    if framed {
        Block::default().borders(Borders::ALL).inner(area)
    } else {
        area
    }
}

/// Resolve a pre-parsed border color config to an actual `Color` using the
/// current palette.  Palette tokens are looked up; literal colours were
/// already parsed at config-load time and are returned directly.
fn resolve_border_color(config: &crate::config::BorderColorConfig, p: &Palette) -> Color {
    if let Some(color) = config.parsed {
        return color;
    }
    p.resolve_token(&config.raw)
        .unwrap_or_else(|| crate::config::parse_color(&config.raw))
}

/// Resolve the correct junction character for a cell based on which of its
/// four cardinal neighbors also have split lines. `fallback` is returned when
/// no neighbors have lines (a degenerate single-cell line).
fn junction_char(
    v_above: bool,
    v_below: bool,
    h_left: bool,
    h_right: bool,
    vert_ch: char,
    horiz_ch: char,
    thick: bool,
    fallback: char,
) -> char {
    let thin_ch = match (v_above, v_below, h_left, h_right) {
        // Simple lines.
        (true, true, false, false) => vert_ch,
        (false, false, true, true) => horiz_ch,
        // T-junction: vertical line passes through, horizontal on right.
        (true, true, false, true) => '├',
        // T-junction: vertical line passes through, horizontal on left.
        (true, true, true, false) => '┤',
        // T-junction: horizontal line passes through, vertical below.
        (false, true, true, true) => '┬',
        // T-junction: horizontal line passes through, vertical above.
        (true, false, true, true) => '┴',
        // Full crossing.
        (true, true, true, true) => '┼',
        // Edges: vertical continuation at pane boundary.
        (true, false, false, false) | (false, true, false, false) => vert_ch,
        // Edges: horizontal continuation at pane boundary.
        (false, false, true, false) | (false, false, false, true) => horiz_ch,
        // Degenerate: no neighbor lines.
        (false, false, false, false) => fallback,
        // Catch-all for unusual patterns (corner elements).
        _ => '┼',
    };
    if thick {
        match thin_ch {
            '├' => '┣',
            '┤' => '┫',
            '┬' => '┳',
            '┴' => '┻',
            '┼' => '╋',
            other => other,
        }
    } else {
        thin_ch
    }
}

/// Convert a PaneBorderStyle config value to a ratatui border::Set.
fn border_set_for_style(style: &PaneBorderStyle) -> ratatui::symbols::border::Set<'_> {
    match style {
        PaneBorderStyle::Thick => ratatui::symbols::border::THICK,
        PaneBorderStyle::Plain => ratatui::symbols::border::PLAIN,
    }
}

/// Compute the pane inner rect for Line mode, shrinking edges that share a split line.
fn inner_rect_for_line_mode(pane_rect: Rect, splits: &[SplitBorder]) -> Rect {
    let mut inner = pane_rect;
    for split in splits {
        match split.direction {
            ratatui::layout::Direction::Horizontal => {
                // Vertical split line at x = pos
                if split.pos == pane_rect.x {
                    inner.x += 1;
                    inner.width = inner.width.saturating_sub(1);
                } else if split.pos == pane_rect.x + pane_rect.width {
                    inner.width = inner.width.saturating_sub(1);
                }
            }
            ratatui::layout::Direction::Vertical => {
                // Horizontal split line at y = pos
                if split.pos == pane_rect.y {
                    inner.y += 1;
                    inner.height = inner.height.saturating_sub(1);
                } else if split.pos == pane_rect.y + pane_rect.height {
                    inner.height = inner.height.saturating_sub(1);
                }
            }
        }
    }
    inner
}

fn runtime_for_tab_pane<'a>(
    terminal_runtimes: &'a TerminalRuntimeRegistry,
    tab: &'a crate::workspace::Tab,
    pane_id: crate::layout::PaneId,
) -> Option<(&'a crate::terminal::TerminalId, &'a TerminalRuntime)> {
    let terminal_id = tab.terminal_id(pane_id)?;
    #[cfg(test)]
    if let Some(runtime) = tab.runtimes.get(&pane_id) {
        return Some((terminal_id, runtime));
    }
    terminal_runtimes
        .get(terminal_id)
        .map(|runtime| (terminal_id, runtime))
}

fn stable_scrollbar_gutter(rt: &TerminalRuntime, pane_inner: Rect) -> (Rect, Option<Rect>) {
    let inner_rect = stable_terminal_inner_rect(pane_inner);
    if inner_rect == pane_inner {
        return (inner_rect, None);
    }
    let gutter = Rect::new(
        pane_inner.x + pane_inner.width.saturating_sub(1),
        pane_inner.y,
        1,
        pane_inner.height,
    );
    let scrollbar_rect = rt
        .scroll_metrics()
        .filter(|metrics| should_show_scrollbar(*metrics))
        .map(|_| gutter);

    (inner_rect, scrollbar_rect)
}

/// Resize every visible runtime in a tab to the geometry it would receive if the tab were selected.
pub(super) fn resize_tab_panes(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    tab: &crate::workspace::Tab,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let multi_pane = tab.layout.pane_count() > 1;
    let line_mode = multi_pane && app.pane_border_mode == PaneBorderMode::Line;
    let splits = if line_mode {
        Some(tab.layout.splits(area))
    } else {
        None
    };

    if tab.zoomed {
        let focused_id = tab.layout.focused();
        if let Some((terminal_id, rt)) = runtime_for_tab_pane(terminal_runtimes, tab, focused_id) {
            let pane_inner = if multi_pane && app.pane_border_mode == PaneBorderMode::Box {
                pane_inner_rect(area, true)
            } else {
                area
            };
            let inner_rect = stable_terminal_inner_rect(pane_inner);
            if !app.direct_attach_resize_locks.contains(terminal_id) {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
        return;
    }

    for info in tab.layout.panes(area) {
        let pane_inner = if multi_pane && app.pane_border_mode == PaneBorderMode::Box {
            Block::default().borders(Borders::ALL).inner(info.rect)
        } else if line_mode {
            if let Some(ref splits) = splits {
                inner_rect_for_line_mode(info.rect, splits)
            } else {
                info.rect
            }
        } else {
            area
        };

        if let Some((terminal_id, rt)) = runtime_for_tab_pane(terminal_runtimes, tab, info.id) {
            let inner_rect = stable_terminal_inner_rect(pane_inner);
            if !app.direct_attach_resize_locks.contains(terminal_id) {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
    }
}

/// Compute pane layout info and optionally resize pane runtimes to match.
pub(super) fn compute_pane_infos(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
    split_borders: &[SplitBorder],
) -> Vec<PaneInfo> {
    let Some(ws_idx) = app.active else {
        return Vec::new();
    };
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return Vec::new();
    };

    let multi_pane = ws.layout.pane_count() > 1;
    let line_mode = multi_pane && app.pane_border_mode == PaneBorderMode::Line;

    if ws.zoomed {
        let focused_id = ws.layout.focused();
        let framed = multi_pane && app.pane_border_mode == PaneBorderMode::Box;
        let pane_inner = pane_inner_rect(area, framed);
        let mut inner_rect = pane_inner;
        let mut scrollbar_rect = None;
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, focused_id) {
            (inner_rect, scrollbar_rect) = stable_scrollbar_gutter(rt, pane_inner);
            if resize_panes
                && ws.terminal_id(focused_id).is_some_and(|terminal_id| {
                    !app.direct_attach_resize_locks.contains(terminal_id)
                })
            {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
        return vec![PaneInfo {
            id: focused_id,
            rect: area,
            inner_rect,
            scrollbar_rect,
            is_focused: true,
        }];
    }

    let mut pane_infos = ws.layout.panes(area);

    for info in &mut pane_infos {
        let pane_inner = if multi_pane && app.pane_border_mode == PaneBorderMode::Box {
            Block::default().borders(Borders::ALL).inner(info.rect)
        } else if line_mode {
            inner_rect_for_line_mode(info.rect, split_borders)
        } else {
            area
        };

        let mut inner_rect = pane_inner;
        let mut scrollbar_rect = None;
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
            (inner_rect, scrollbar_rect) = stable_scrollbar_gutter(rt, pane_inner);
            if resize_panes
                && ws.terminal_id(info.id).is_some_and(|terminal_id| {
                    !app.direct_attach_resize_locks.contains(terminal_id)
                })
            {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }

        info.inner_rect = inner_rect;
        info.scrollbar_rect = scrollbar_rect;
    }

    pane_infos
}

/// Draw single-character split lines between panes in Line mode.
/// Uses a two-pass approach: first collect which cells have vertical and
/// horizontal lines, then determine the correct junction character for each cell
/// by checking all four cardinal neighbors.
///
/// Walk a single row or column of a split line, inserting cells into `cells`
/// and (when the position falls within the focused pane's extent) `active_cells`.
fn fill_split_line(
    cells: &mut HashSet<(u16, u16)>,
    active_cells: &mut HashSet<(u16, u16)>,
    range: std::ops::Range<u16>,
    fixed: u16,
    direction: ratatui::layout::Direction,
    buf_width: u16,
    buf_height: u16,
    focused_rect: Option<Rect>,
) {
    let vertical_line = direction == ratatui::layout::Direction::Horizontal;
    for var in range {
        let (x, y) = if vertical_line {
            (fixed, var)
        } else {
            (var, fixed)
        };
        if x >= buf_width || y >= buf_height {
            continue;
        }
        cells.insert((x, y));
        let Some(fr) = focused_rect else {
            continue;
        };
        let in_extent = if vertical_line {
            var >= fr.y && var <= fr.y + fr.height
        } else {
            var >= fr.x && var <= fr.x + fr.width
        };
        if in_extent {
            active_cells.insert((x, y));
        }
    }
}

fn render_split_lines(
    app: &AppState,
    frame: &mut Frame,
    active_color: &Color,
    inactive_color: &Color,
    border_set: &ratatui::symbols::border::Set<'_>,
) {
    let buf = frame.buffer_mut();

    let mut vertical_cells: HashSet<(u16, u16)> = HashSet::new();
    let mut horizontal_cells: HashSet<(u16, u16)> = HashSet::new();
    // Cells that touch the focused pane → rendered with active_color.
    let mut active_cells: HashSet<(u16, u16)> = HashSet::new();

    // Find the focused pane so we can determine which split lines touch it.
    let focused_info = app.view.pane_infos.iter().find(|info| info.is_focused);

    for split in &app.view.split_borders {
        // The focused pane's rect when this split borders the focused pane.
        let focused_rect = focused_info.and_then(|fi| {
            let touches = match split.direction {
                ratatui::layout::Direction::Horizontal => {
                    split.pos == fi.rect.x || split.pos == fi.rect.x + fi.rect.width
                }
                ratatui::layout::Direction::Vertical => {
                    split.pos == fi.rect.y || split.pos == fi.rect.y + fi.rect.height
                }
            };
            if touches {
                Some(fi.rect)
            } else {
                None
            }
        });

        match split.direction {
            ratatui::layout::Direction::Horizontal => {
                fill_split_line(
                    &mut vertical_cells,
                    &mut active_cells,
                    split.area.y..split.area.y + split.area.height,
                    split.pos,
                    ratatui::layout::Direction::Horizontal,
                    buf.area.width,
                    buf.area.height,
                    focused_rect,
                );
            }
            ratatui::layout::Direction::Vertical => {
                fill_split_line(
                    &mut horizontal_cells,
                    &mut active_cells,
                    split.area.x..split.area.x + split.area.width,
                    split.pos,
                    ratatui::layout::Direction::Vertical,
                    buf.area.width,
                    buf.area.height,
                    focused_rect,
                );
            }
        }
    }

    // Draw every cell that has at least one line, using the correct
    // junction character determined by checking all four neighbors.
    let vert_ch = border_set.vertical_left.chars().next().unwrap_or('│');
    let horiz_ch = border_set.horizontal_top.chars().next().unwrap_or('─');
    let thick = app.pane_border_style == PaneBorderStyle::Thick;

    for &(x, y) in vertical_cells.iter().chain(horizontal_cells.iter()) {
        let v_above = y > 0 && vertical_cells.contains(&(x, y.saturating_sub(1)));
        let v_below = y + 1 < buf.area.height && vertical_cells.contains(&(x, y + 1));
        let h_left = x > 0 && horizontal_cells.contains(&(x.saturating_sub(1), y));
        let h_right = x + 1 < buf.area.width && horizontal_cells.contains(&(x + 1, y));

        let fallback = if vertical_cells.contains(&(x, y)) {
            vert_ch
        } else {
            horiz_ch
        };
        let ch = junction_char(
            v_above, v_below, h_left, h_right, vert_ch, horiz_ch, thick, fallback,
        );

        let cell_style = if active_cells.contains(&(x, y)) {
            Style::default().fg(*active_color)
        } else {
            Style::default().fg(*inactive_color)
        };

        buf[(x, y)].set_char(ch);
        buf[(x, y)].set_style(cell_style);
    }
}

pub(super) fn render_panes(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(ws_idx) = app.active else {
        render_empty(app, frame, area);
        return;
    };
    let Some(ws) = app.workspaces.get(ws_idx) else {
        render_empty(app, frame, area);
        return;
    };

    let multi_pane = ws.layout.pane_count() > 1;
    let terminal_active = app.mode == Mode::Terminal;
    let box_mode = multi_pane && app.pane_border_mode == PaneBorderMode::Box;
    let line_mode = multi_pane && app.pane_border_mode == PaneBorderMode::Line;

    // Resolve border colors once per frame.
    let active_border_color = resolve_border_color(&app.pane_border_active_color, &app.palette);
    let inactive_border_color = resolve_border_color(&app.pane_border_inactive_color, &app.palette);
    let configured_border_set = border_set_for_style(&app.pane_border_style);

    for info in &app.view.pane_infos {
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
            if box_mode {
                let border_style = if info.is_focused {
                    Style::default().fg(active_border_color)
                } else {
                    Style::default().fg(inactive_border_color)
                };

                // Use the configured border style only when the user is
                // actively in terminal mode on the focused pane.  Otherwise
                // fall back to plain so the border visually recedes.
                let border_set = if info.is_focused && terminal_active {
                    configured_border_set
                } else {
                    ratatui::symbols::border::PLAIN
                };

                let mut block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .border_set(border_set);
                if let Some(title) = ws
                    .pane_state(info.id)
                    .and_then(|pane| app.terminals.get(&pane.attached_terminal_id))
                    .and_then(|terminal| {
                        terminal.border_label(app.show_agent_labels_on_pane_borders)
                    })
                    .and_then(|label| pane_border_title(&label, info.rect.width))
                {
                    block = block.title(Line::from(Span::styled(title, border_style)));
                }
                frame.render_widget(block, info.rect);
            }

            let show_cursor = info.is_focused
                && terminal_active
                && !pane_is_scrolled_back(rt)
                && app.pane_exposes_host_cursor(ws_idx, info.id);
            rt.render(frame, info.inner_rect, show_cursor);
            render_pane_scrollbar(app, frame, info, rt);

            let should_dim = !info.is_focused && multi_pane && !terminal_active;
            if should_dim {
                let inner = info.inner_rect;
                let buf = frame.buffer_mut();
                for y in inner.y..inner.y + inner.height {
                    for x in inner.x..inner.x + inner.width {
                        let cell = &mut buf[(x, y)];
                        cell.set_style(cell.style().add_modifier(Modifier::DIM));
                    }
                }
            }

            render_selection_highlight(
                &app.selection,
                frame,
                info.id,
                info.inner_rect,
                rt.scroll_metrics(),
                &app.palette,
                app.host_terminal_theme,
            );
            render_copy_mode_cursor(app, frame, info);
        }
    }

    // In Line mode, draw single-character split lines between panes.
    if line_mode {
        render_split_lines(
            app,
            frame,
            &active_border_color,
            &inactive_border_color,
            &configured_border_set,
        );
    }
}

fn render_copy_mode_cursor(app: &AppState, frame: &mut Frame, info: &PaneInfo) {
    if app.mode != Mode::Copy {
        return;
    }
    let Some(copy_mode) = app.copy_mode else {
        return;
    };
    if copy_mode.pane_id != info.id
        || copy_mode.cursor_row >= info.inner_rect.height
        || copy_mode.cursor_col >= info.inner_rect.width
    {
        return;
    }

    let x = info.inner_rect.x + copy_mode.cursor_col;
    let y = info.inner_rect.y + copy_mode.cursor_row;
    let cell = &mut frame.buffer_mut()[(x, y)];
    cell.set_style(
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_selection_highlight(
    selection: &Option<crate::selection::Selection>,
    frame: &mut Frame,
    pane_id: crate::layout::PaneId,
    inner: Rect,
    scroll_metrics: Option<crate::pane::ScrollMetrics>,
    p: &Palette,
    host_theme: crate::terminal_theme::TerminalTheme,
) {
    if let Some(sel) = selection {
        if sel.is_visible() && sel.pane_id == pane_id {
            let buf = frame.buffer_mut();
            let style = automatic_selection_style(p, host_theme);
            for y in 0..inner.height {
                for x in 0..inner.width {
                    if sel.contains(y, x, scroll_metrics) {
                        let cell = &mut buf[(inner.x + x, inner.y + y)];
                        cell.set_style(style);
                    }
                }
            }
        }
    }
}

type Rgb = (u8, u8, u8);

fn automatic_selection_style(
    p: &Palette,
    host_theme: crate::terminal_theme::TerminalTheme,
) -> Style {
    let bg = automatic_selection_bg(p, host_theme);
    Style::reset().fg(selection_fg_for_bg(bg, p)).bg(bg)
}

fn automatic_selection_bg(p: &Palette, host_theme: crate::terminal_theme::TerminalTheme) -> Color {
    let Some(background) = host_theme.background.map(terminal_theme_to_rgb) else {
        return selection_palette_background(p);
    };

    let target = if relative_luminance(background) < 0.5 {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    };
    let selected = mix_rgb(background, target, 0.28);
    Color::Rgb(selected.0, selected.1, selected.2)
}

fn selection_palette_background(p: &Palette) -> Color {
    if p.panel_bg == Color::Reset {
        p.surface_dim
    } else {
        p.panel_bg
    }
}

fn terminal_theme_to_rgb(color: crate::terminal_theme::RgbColor) -> Rgb {
    (color.r, color.g, color.b)
}

fn selection_fg_for_bg(bg: Color, p: &Palette) -> Color {
    color_to_rgb(bg)
        .map(|bg| {
            if relative_luminance(bg) < 0.5 {
                Color::White
            } else {
                Color::Black
            }
        })
        .unwrap_or_else(|| panel_contrast_fg(p))
}

fn mix_rgb(base: Rgb, target: Rgb, amount: f32) -> Rgb {
    fn channel(base: u8, target: u8, amount: f32) -> u8 {
        (f32::from(base) + (f32::from(target) - f32::from(base)) * amount).round() as u8
    }
    (
        channel(base.0, target.0, amount),
        channel(base.1, target.1, amount),
        channel(base.2, target.2, amount),
    )
}

fn relative_luminance(color: Rgb) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.0) + 0.7152 * channel(color.1) + 0.0722 * channel(color.2)
}

fn color_to_rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Reset => None,
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(_) => None,
    }
}

fn render_empty(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    let lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "  No workspaces yet",
            Style::default().fg(p.overlay0),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  A workspace is one project context.",
            Style::default().fg(p.overlay1),
        )),
        Line::from(Span::styled(
            "  Its root pane (top-left) sets the default repo or folder name.",
            Style::default().fg(p.overlay1),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Press ", Style::default().fg(p.overlay0)),
            Span::styled(
                app.keybinds
                    .new_workspace
                    .label()
                    .unwrap_or_else(|| "unset".to_string()),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to create one", Style::default().fg(p.overlay0)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.surface_dim)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneId;
    use crate::selection::Selection;
    use crate::terminal::TerminalRuntime;
    use crate::workspace::Workspace;

    #[test]
    fn pane_border_title_trims_and_truncates() {
        assert_eq!(
            pane_border_title(" claude ", 20).as_deref(),
            Some(" claude ")
        );
        assert_eq!(pane_border_title("", 20), None);
        assert_eq!(pane_border_title("abcdef", 8).as_deref(), Some(" abc… "));
        assert_eq!(pane_border_title("abcdef", 4), None);
    }

    #[tokio::test]
    async fn pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
            &[],
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn zoomed_pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        workspace.zoomed = true;
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
            &[],
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn zoomed_multi_pane_keeps_border_space() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let focused_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.zoomed = true;
        workspace.tabs[0].runtimes.insert(
            focused_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
            &[],
        );
        let info = &infos[0];

        assert_eq!(info.id, focused_pane);
        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));
    }

    #[tokio::test]
    async fn tiny_pane_does_not_reserve_scrollbar_gutter() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(4, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 4, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
            &[],
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, area);
    }

    #[tokio::test]
    async fn pane_scrollbar_reserves_last_column_from_terminal_area() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(
                40,
                8,
                1024,
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            ),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
            &[],
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, Some(Rect::new(49, 3, 1, 8)));
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[test]
    fn selection_highlight_uses_one_uniform_style() {
        let palette = Palette::catppuccin();
        let host_theme = crate::terminal_theme::TerminalTheme {
            foreground: None,
            background: Some(crate::terminal_theme::RgbColor {
                r: 12,
                g: 14,
                b: 16,
            }),
        };
        let expected_style = automatic_selection_style(&palette, host_theme);
        let selection = Some(Selection::range(PaneId::from_raw(1), 0, 0, 2, None));
        let backend = ratatui::backend::TestBackend::new(4, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                buf[(0, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(10, 220, 120))
                        .bg(Color::Black),
                );
                buf[(1, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(220, 180, 40))
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
                buf[(2, 0)].set_style(Style::default().fg(Color::Blue).bg(Color::Reset));
                render_selection_highlight(
                    &selection,
                    frame,
                    PaneId::from_raw(1),
                    Rect::new(0, 0, 4, 1),
                    None,
                    &palette,
                    host_theme,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let first = buffer[(0, 0)].style();
        let second = buffer[(1, 0)].style();
        let third = buffer[(2, 0)].style();

        assert_eq!(first.fg, expected_style.fg);
        assert_eq!(second.fg, expected_style.fg);
        assert_eq!(third.fg, expected_style.fg);
        assert_eq!(first.bg, expected_style.bg);
        assert_eq!(second.bg, expected_style.bg);
        assert_eq!(third.bg, expected_style.bg);
        assert_eq!(first.add_modifier, expected_style.add_modifier);
        assert_eq!(second.add_modifier, expected_style.add_modifier);
        assert_eq!(third.add_modifier, expected_style.add_modifier);
        assert!(!second.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn automatic_selection_background_uses_host_background() {
        let bg = automatic_selection_bg(
            &Palette::terminal(),
            crate::terminal_theme::TerminalTheme {
                foreground: Some(crate::terminal_theme::RgbColor {
                    r: 230,
                    g: 230,
                    b: 230,
                }),
                background: Some(crate::terminal_theme::RgbColor {
                    r: 12,
                    g: 14,
                    b: 16,
                }),
            },
        );

        let Color::Rgb(r, g, b) = bg else {
            panic!("selection background should resolve to rgb");
        };
        assert!(relative_luminance((r, g, b)) > relative_luminance((12, 14, 16)));
    }

    #[test]
    fn resolve_border_color_palette_tokens() {
        let p = Palette::catppuccin();
        let accent = crate::config::BorderColorConfig::from_string("accent");
        let overlay0 = crate::config::BorderColorConfig::from_string("overlay0");
        let green = crate::config::BorderColorConfig::from_string("green");
        let text = crate::config::BorderColorConfig::from_string("text");
        assert_eq!(resolve_border_color(&accent, &p), p.accent);
        assert_eq!(resolve_border_color(&overlay0, &p), p.overlay0);
        assert_eq!(resolve_border_color(&green, &p), p.green);
        assert_eq!(resolve_border_color(&text, &p), p.text);
    }

    #[test]
    fn resolve_border_color_falls_back_to_literal() {
        let p = Palette::catppuccin();
        // Palette token names take precedence over terminal color names.
        let red = crate::config::BorderColorConfig::from_string("red");
        assert_eq!(resolve_border_color(&red, &p), p.red);
        // Unknown names fall through to literal color parsing.
        let hex = crate::config::BorderColorConfig::from_string("#ff0000");
        assert_eq!(resolve_border_color(&hex, &p), Color::Rgb(255, 0, 0));
        let named = crate::config::BorderColorConfig::from_string("lightred");
        assert_eq!(resolve_border_color(&named, &p), Color::LightRed);
    }

    #[test]
    fn inner_rect_for_line_mode_shrinks_split_edges() {
        use ratatui::layout::{Direction, Rect};

        // Simple 2-pane horizontal split (left/right)
        let splits = vec![SplitBorder {
            pos: 50,
            direction: Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(0, 0, 100, 24),
            path: vec![],
        }];

        let left_rect = Rect::new(0, 0, 50, 24);
        let right_rect = Rect::new(50, 0, 50, 24);

        let left_inner = inner_rect_for_line_mode(left_rect, &splits);
        assert_eq!(left_inner, Rect::new(0, 0, 49, 24));

        let right_inner = inner_rect_for_line_mode(right_rect, &splits);
        assert_eq!(right_inner, Rect::new(51, 0, 49, 24));
    }

    #[test]
    fn inner_rect_for_line_mode_t_junction() {
        use ratatui::layout::{Direction, Rect};

        // T-layout: left side split into top/bottom, right side full height.
        // Left-top (A): (0,0,50,12), Left-bottom (C): (0,12,50,12), Right (B): (50,0,50,24)
        let splits = vec![
            SplitBorder {
                pos: 50,
                direction: Direction::Horizontal,
                ratio: 0.5,
                area: Rect::new(0, 0, 100, 24),
                path: vec![],
            },
            SplitBorder {
                pos: 12,
                direction: Direction::Vertical,
                ratio: 0.5,
                area: Rect::new(0, 0, 50, 24),
                path: vec![false],
            },
        ];

        // Pane A: (0, 0, 50, 12) → right edge shrunk (split at 50), bottom edge shrunk (split at 12)
        let a = inner_rect_for_line_mode(Rect::new(0, 0, 50, 12), &splits);
        assert_eq!(a, Rect::new(0, 0, 49, 11));

        // Pane C: (0, 12, 50, 12) → right edge shrunk (split at 50), top edge shrunk (split at 12)
        let c = inner_rect_for_line_mode(Rect::new(0, 12, 50, 12), &splits);
        assert_eq!(c, Rect::new(0, 13, 49, 11));

        // Pane B: (50, 0, 50, 24) → left edge shrunk (split at 50)
        let b = inner_rect_for_line_mode(Rect::new(50, 0, 50, 24), &splits);
        assert_eq!(b, Rect::new(51, 0, 49, 24));
    }

    #[test]
    fn junction_char_simple_lines() {
        let v = '│';
        let h = '─';
        // Vertical pass-through.
        assert_eq!(
            junction_char(true, true, false, false, v, h, false, '.'),
            '│'
        );
        // Horizontal pass-through.
        assert_eq!(
            junction_char(false, false, true, true, v, h, false, '.'),
            '─'
        );
    }

    #[test]
    fn junction_char_t_junctions() {
        let v = '│';
        let h = '─';
        // Thin T-junctions.
        assert_eq!(
            junction_char(true, true, false, true, v, h, false, '.'),
            '├'
        );
        assert_eq!(
            junction_char(true, true, true, false, v, h, false, '.'),
            '┤'
        );
        assert_eq!(
            junction_char(false, true, true, true, v, h, false, '.'),
            '┬'
        );
        assert_eq!(
            junction_char(true, false, true, true, v, h, false, '.'),
            '┴'
        );
        // Thick T-junctions.
        assert_eq!(junction_char(true, true, false, true, v, h, true, '.'), '┣');
        assert_eq!(junction_char(true, true, true, false, v, h, true, '.'), '┫');
        assert_eq!(junction_char(false, true, true, true, v, h, true, '.'), '┳');
        assert_eq!(junction_char(true, false, true, true, v, h, true, '.'), '┻');
    }

    #[test]
    fn junction_char_full_crossing() {
        let v = '│';
        let h = '─';
        assert_eq!(junction_char(true, true, true, true, v, h, false, '.'), '┼');
        assert_eq!(junction_char(true, true, true, true, v, h, true, '.'), '╋');
    }

    #[test]
    fn junction_char_edge_single_direction() {
        let v = '│';
        let h = '─';
        // Vertical only — one end terminates at pane boundary.
        assert_eq!(
            junction_char(true, false, false, false, v, h, false, '.'),
            '│'
        );
        assert_eq!(
            junction_char(false, true, false, false, v, h, false, '.'),
            '│'
        );
        // Horizontal only.
        assert_eq!(
            junction_char(false, false, true, false, v, h, false, '.'),
            '─'
        );
        assert_eq!(
            junction_char(false, false, false, true, v, h, false, '.'),
            '─'
        );
    }

    #[test]
    fn junction_char_degenerate_uses_fallback() {
        let v = '│';
        let h = '─';
        assert_eq!(
            junction_char(false, false, false, false, v, h, false, '│'),
            '│'
        );
        assert_eq!(
            junction_char(false, false, false, false, v, h, false, '─'),
            '─'
        );
    }

    #[test]
    fn junction_char_catch_all_patterns() {
        let v = '│';
        let h = '─';
        // Corner patterns with no continuation on one side.
        // thin.
        assert_eq!(
            junction_char(false, true, false, true, v, h, false, '.'),
            '┼'
        );
        assert_eq!(
            junction_char(false, true, true, false, v, h, false, '.'),
            '┼'
        );
        assert_eq!(
            junction_char(true, false, false, true, v, h, false, '.'),
            '┼'
        );
        assert_eq!(
            junction_char(true, false, true, false, v, h, false, '.'),
            '┼'
        );
        // thick.
        assert_eq!(
            junction_char(false, true, false, true, v, h, true, '.'),
            '╋'
        );
        assert_eq!(
            junction_char(false, true, true, false, v, h, true, '.'),
            '╋'
        );
        assert_eq!(
            junction_char(true, false, false, true, v, h, true, '.'),
            '╋'
        );
        assert_eq!(
            junction_char(true, false, true, false, v, h, true, '.'),
            '╋'
        );
    }
}
