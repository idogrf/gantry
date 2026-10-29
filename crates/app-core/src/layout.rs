//! Pixel-to-grid size calculations.

/// Smallest grid a pane gets. Reflowing to fewer columns multiplies the lines of scrollback until the oldest fall
/// off, so a narrower pane clips its grid instead.
pub const MIN_COLS: u16 = 20;
/// Smallest number of rows a pane gets.
pub const MIN_ROWS: u16 = 5;

/// How many whole cells fit in `width`×`height` pixels, as `(cols, rows)`. Never smaller than `MIN_COLS`×`MIN_ROWS`.
pub fn grid_dims(width: f32, height: f32, cell_width: f32, cell_height: f32) -> (u16, u16) {
    let fit = |space: f32, cell: f32, min: u16| {
        let count = (space / cell).floor();
        if count.is_finite() && count >= min as f32 { count.min(u16::MAX as f32) as u16 } else { min }
    };
    (fit(width, cell_width, MIN_COLS), fit(height, cell_height, MIN_ROWS))
}

/// A pane's grid in the GUI and the grid last sent to the daemon for it, as `(cols, rows)`.
///
/// They can differ: a resync rebuilds the GUI's terminal at the daemon's size when the resync was made, which can be
/// older than a resize still on its way. The daemon ends at the last size sent, so a size is sent whenever it differs
/// from that one, whatever the GUI's terminal holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSize {
    /// Size of the GUI's terminal.
    pub grid: (u16, u16),
    /// Size last sent to the daemon for the attached pane; `None` before the first.
    pub sent: Option<(u16, u16)>,
}

impl PaneSize {
    /// A terminal of `grid`, nothing sent yet.
    pub fn new(grid: (u16, u16)) -> PaneSize {
        PaneSize { grid, sent: None }
    }

    /// Whether the GUI's terminal must be resized to fit `dims`.
    pub fn needs_resize(&self, dims: (u16, u16)) -> bool {
        dims != self.grid
    }

    /// Whether the daemon must be told `dims`.
    pub fn needs_send(&self, dims: (u16, u16)) -> bool {
        self.sent != Some(dims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divides_space_into_whole_cells() {
        assert_eq!(grid_dims(805.0, 370.0, 8.0, 18.0), (100, 20));
    }

    #[test]
    fn tiny_or_invalid_space_keeps_the_minimum_grid() {
        assert_eq!(grid_dims(0.0, 0.0, 8.0, 18.0), (MIN_COLS, MIN_ROWS));
        assert_eq!(grid_dims(80.0, 36.0, 8.0, 18.0), (20, 5));
        assert_eq!(grid_dims(500.0, 500.0, 0.0, 0.0), (MIN_COLS, MIN_ROWS));
        assert_eq!(grid_dims(f32::NAN, 180.0, 8.0, 18.0), (MIN_COLS, 10));
    }

    #[test]
    fn a_stale_resync_does_not_hide_a_size_from_the_daemon() {
        // Reopening the window: the daemon replays at the size it last had (26 rows, with the last prompt card), the
        // first layout before the card shows sends 30, then the replay arrives, then the card shows: 26 again.
        let mut size = PaneSize::new((80, 24));
        assert!(size.needs_send((62, 30)));
        size.grid = (62, 30);
        size.sent = Some((62, 30));
        size.grid = (62, 26);
        assert!(!size.needs_resize((62, 26)));
        assert!(size.needs_send((62, 26)), "the daemon is at 30 and must be told 26");
    }

    #[test]
    fn an_unchanged_size_is_not_sent_again() {
        let size = PaneSize { grid: (62, 26), sent: Some((62, 26)) };
        assert!(!size.needs_resize((62, 26)));
        assert!(!size.needs_send((62, 26)));
    }
}
