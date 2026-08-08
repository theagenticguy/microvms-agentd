//! The interactive surface (CLI-1), and the rule that keeps it off a pipe.
//!
//! # It draws only when stdout is a terminal
//!
//! `cli.py` has no `isatty` anywhere and is purely flag-driven; this is the one behaviour the
//! Rust port adds rather than ports. The rule lives in
//! [`crate::envelope::resolve_format`] and this module is only reached for
//! [`crate::envelope::Format::Tui`], so there is no path from a piped invocation to a frame —
//! and deliberately no `--tui` flag, because a flag would create one.
//!
//! # `restore` runs on every path, including a failure
//!
//! [`ratatui::init`] puts the terminal in raw mode and on the alternate screen. Leaving it there
//! wedges the caller's shell: no echo, no prompt, and the fix is `reset(1)` if they know to try
//! it. So every function here captures its result, calls
//! [`ratatui::restore`], and *then* returns — the pattern the ratatui README models, and the
//! reason it is written that way rather than with `?` inside the drawing block.
//!
//! `init` also installs a panic hook that restores before the message prints, which covers the
//! one path a `let result = ...; restore(); result` cannot. Its docs are explicit that it must be
//! installed *after* any other hook, which is why nothing here installs one.
//!
//! # These are frames, not applications
//!
//! Each function draws once and returns. There is no event loop, no key handling, and no
//! alternate mode a caller can get stuck in: `microvm ls` is a command that prints and exits,
//! and a full-screen application that waits for `q` would be a worse `ls`. What the TUI buys is
//! alignment, colour on the columns that matter — a leak is red — and a table that does not
//! reflow when a value is long.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

/// A table's rows, already stringified, plus the columns' headers.
///
/// Strings rather than a generic row type, because every caller has already rendered its values
/// for the plain path — and a TUI that re-derived them would be a second rendering that can
/// disagree with the first.
pub struct Grid {
    pub title: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Row indices to draw in the alarm colour. Used for a leak, which is the one thing on any
    /// of these surfaces the operator must not skim past.
    pub alarm_rows: Vec<usize>,
    /// A closing line under the table — a total, a count, a warning.
    pub footer: Option<String>,
}

impl Grid {
    pub fn new(title: impl Into<String>, headers: Vec<String>) -> Self {
        Self {
            title: title.into(),
            headers,
            rows: Vec::new(),
            alarm_rows: Vec::new(),
            footer: None,
        }
    }

    #[must_use]
    pub fn with_row(mut self, row: Vec<String>) -> Self {
        self.rows.push(row);
        self
    }

    /// Marks the last pushed row as an alarm.
    #[must_use]
    pub fn alarming(mut self) -> Self {
        if !self.rows.is_empty() {
            self.alarm_rows.push(self.rows.len() - 1);
        }
        self
    }

    #[must_use]
    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }
}

/// Draws `grid` once and restores the terminal on every path.
///
/// Returns whether the frame was drawn: `false` means the terminal could not be initialised, and
/// the caller falls back to plain text rather than printing nothing. That fallback is what makes
/// the TUI an enhancement rather than a dependency — a `TERM` the backend does not understand
/// must not cost a caller their output.
pub fn draw(grid: &Grid) -> bool {
    // `try_init` rather than `init`: `init` panics on setup failure by design, and a panic here
    // would replace a perfectly renderable answer with a stack trace.
    let Ok(mut terminal) = ratatui::try_init() else {
        return false;
    };
    let result = terminal.draw(|frame| render(frame, grid));
    // Before the return, on both paths. See the module docs.
    ratatui::restore();
    result.is_ok()
}

/// One frame: a bordered table with an optional footer.
fn render(frame: &mut ratatui::Frame<'_>, grid: &Grid) {
    let area = frame.area();
    let [table_area, footer_area] = Layout::vertical([
        Constraint::Min(3),
        // One line, or none when there is no footer — rather than an empty line that makes the
        // table shift by one depending on content.
        Constraint::Length(if grid.footer.is_some() { 2 } else { 0 }),
    ])
    .areas(area);

    let widths: Vec<Constraint> = column_widths(grid);
    let header = Row::new(
        grid.headers
            .iter()
            .map(|text| Cell::from(text.clone()))
            .collect::<Vec<_>>(),
    )
    .style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Cyan),
    );
    let rows: Vec<Row<'_>> = grid
        .rows
        .iter()
        .enumerate()
        .map(|(index, cells)| {
            let row = Row::new(
                cells
                    .iter()
                    .map(|text| Cell::from(text.clone()))
                    .collect::<Vec<_>>(),
            );
            if grid.alarm_rows.contains(&index) {
                // Red, because this row names something that is still billing.
                row.style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            } else {
                row
            }
        })
        .collect();

    frame.render_widget(
        Table::new(rows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .title(grid.title.clone()),
        ),
        table_area,
    );
    if let Some(footer) = &grid.footer {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                footer.clone(),
                Style::default().fg(Color::DarkGray),
            )])),
            footer_area,
        );
    }
}

/// A constraint per column, sized to the widest cell.
///
/// Computed rather than fixed, because the values here are ARNs and endpoints whose lengths are
/// nothing like each other — and a fixed layout truncates an identifier, which for a leaked
/// resource is the one string that must survive intact.
fn column_widths(grid: &Grid) -> Vec<Constraint> {
    let mut widths: Vec<u16> = grid
        .headers
        .iter()
        .map(|header| header.chars().count() as u16)
        .collect();
    for row in &grid.rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.chars().count() as u16);
            }
        }
    }
    widths
        .into_iter()
        // Plus one for the gap between columns; a table whose columns touch is unreadable.
        .map(|width| Constraint::Length(width.saturating_add(1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders `grid` into a fixed-size buffer and returns it as lines of text.
    ///
    /// `TestBackend` is what makes this testable at all: it draws into a buffer rather than a
    /// terminal, so the assertions below are about the frame's *content* rather than about
    /// whether a draw call returned. A test that only asserted `draw` returned true would pass
    /// against a function that rendered an empty frame.
    fn rendered(grid: &Grid, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| render(frame, grid))
            .expect("draws into a buffer");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn a_grid() -> Grid {
        Grid::new(
            "outstanding",
            vec!["run".into(), "microvm".into(), "leaked".into()],
        )
        .with_row(vec!["1754524800-1".into(), "mvm-abc123".into(), "-".into()])
        .with_row(vec![
            "1754524801-2".into(),
            "mvm-def456".into(),
            "arn:aws:lambda:us-east-1:123456789012:microvm-image/img".into(),
        ])
        .alarming()
        .with_footer("2 runs, 1 leaked")
    }

    /// Every cell's text reaches the frame, and a long identifier is not truncated.
    ///
    /// The truncation case is the one that matters: for a leaked resource the identifier *is* the
    /// remedy, and a table that clipped it would produce output the operator cannot act on.
    #[test]
    fn every_value_reaches_the_frame_and_a_long_identifier_survives_intact() {
        let grid = a_grid();
        let lines = rendered(&grid, 100, 8);
        let joined = lines.join("\n");
        assert!(joined.contains("outstanding"), "{joined}");
        assert!(joined.contains("mvm-abc123"), "{joined}");
        assert!(
            joined.contains("arn:aws:lambda:us-east-1:123456789012:microvm-image/img"),
            "the leaked identifier must survive whole: {joined}"
        );
        assert!(joined.contains("2 runs, 1 leaked"), "{joined}");
    }

    /// The alarm row is styled differently from a normal one.
    ///
    /// Asserted on the buffer's *style* rather than on its text, because the text is identical
    /// either way — so a version that forgot the styling would pass every content assertion
    /// while making a leak look like a clean row.
    #[test]
    fn a_leaked_row_is_styled_as_an_alarm() {
        let grid = a_grid();
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("a test terminal");
        terminal.draw(|frame| render(frame, &grid)).expect("draws");
        let buffer = terminal.backend().buffer().clone();

        // Find the two data rows by their first cell and compare the colour of that cell.
        let colour_of = |needle: &str| -> Option<Color> {
            for y in 0..8u16 {
                let line: String = (0..100u16)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                if line.contains(needle) {
                    let x = line.find(needle).expect("just matched") as u16;
                    return Some(buffer[(x, y)].style().fg.unwrap_or(Color::Reset));
                }
            }
            None
        };
        let clean = colour_of("1754524800-1").expect("the clean row is drawn");
        let leaked = colour_of("1754524801-2").expect("the leaked row is drawn");
        assert_eq!(leaked, Color::Red, "a leak must be visually distinct");
        assert_ne!(clean, Color::Red, "a clean row must not be");
    }

    /// A grid with no rows still draws its header and its border.
    ///
    /// The empty case is a real one — `microvm ls` with nothing outstanding — and a frame that
    /// collapsed to nothing would read as a failed command.
    #[test]
    fn an_empty_grid_still_draws_its_frame() {
        let grid = Grid::new("outstanding", vec!["run".into(), "microvm".into()]);
        let lines = rendered(&grid, 40, 5);
        let joined = lines.join("\n");
        assert!(joined.contains("outstanding"), "{joined}");
        assert!(joined.contains("run"), "{joined}");
        assert!(
            joined.contains('┌') || joined.contains('|'),
            "a border: {joined}"
        );
    }

    /// Column widths follow the widest cell, header included.
    ///
    /// The header is the case a naive implementation misses: a column whose header is longer than
    /// every value would be sized to the values and clip its own title.
    #[test]
    fn a_column_is_as_wide_as_its_widest_cell_including_the_header() {
        let grid = Grid::new("t", vec!["a-very-long-header".into(), "b".into()])
            .with_row(vec!["x".into(), "a-much-longer-value".into()]);
        let widths = column_widths(&grid);
        assert_eq!(
            widths,
            [
                Constraint::Length("a-very-long-header".len() as u16 + 1),
                Constraint::Length("a-much-longer-value".len() as u16 + 1),
            ]
        );
    }

    /// A grid without a footer reserves no space for one.
    ///
    /// Otherwise the table shifts by two rows depending on whether there is a total, which makes
    /// two runs of the same command look different.
    #[test]
    fn a_grid_without_a_footer_gives_the_whole_area_to_the_table() {
        let with = a_grid();
        let without = Grid {
            footer: None,
            ..a_grid()
        };
        let with_lines = rendered(&with, 100, 10);
        let without_lines = rendered(&without, 100, 10);
        // The footer text is present in one and absent in the other, and the table's own top
        // border is on the same line in both.
        assert!(with_lines.join("\n").contains("2 runs"), "{with_lines:?}");
        assert!(
            !without_lines.join("\n").contains("2 runs"),
            "{without_lines:?}"
        );
        assert_eq!(
            with_lines[0].starts_with('┌'),
            without_lines[0].starts_with('┌'),
            "the table starts in the same place either way"
        );
    }

    /// `alarming()` on an empty grid does nothing rather than panicking.
    ///
    /// The plausible caller mistake: marking an alarm before pushing the row it belongs to.
    #[test]
    fn marking_an_alarm_with_no_rows_is_a_no_op() {
        let grid = Grid::new("t", vec!["a".into()]).alarming();
        assert!(grid.alarm_rows.is_empty());
    }
}
