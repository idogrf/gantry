//! The compact sidebar column: its width, the collapse animation's value and the letter on a tile.

use crate::split_tree::ease_out_cubic;

/// Width of the compact column, in pixels (scaled with the interface font size by the view).
pub const WIDTH: f32 = 56.0;

/// How long the collapse or expand animation runs, in milliseconds.
pub const ANIM_MS: f32 = 250.0;

/// The letter a compact tile shows for `title`: its first letter or digit, uppercased, else `·` (an empty title, a
/// leading space or symbol, an emoji).
pub fn tile_letter(title: &str) -> char {
    match title.chars().next() {
        Some(first) if first.is_alphanumeric() => first.to_uppercase().next().unwrap_or(first),
        _ => '·',
    }
}

/// How compact the sidebar is `elapsed_ms` into an animation from `from` to `to` (0 = expanded, 1 = compact). It eases
/// out and stays at `to` once `ANIM_MS` has passed; an animation that reverses another starts at its current value.
pub fn amount(from: f32, to: f32, elapsed_ms: f32) -> f32 {
    from + (to - from) * ease_out_cubic(elapsed_ms / ANIM_MS)
}

/// The sidebar's width for a compact `amount`, given the width it has when expanded.
pub fn width(expanded: f32, amount: f32) -> f32 {
    expanded + (WIDTH - expanded) * amount
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_letter_is_the_first_letter_or_digit_uppercased() {
        assert_eq!(tile_letter("Fix ssh retry"), 'F');
        assert_eq!(tile_letter("fix"), 'F');
        assert_eq!(tile_letter("7 bugs"), '7');
        assert_eq!(tile_letter("שלום"), 'ש');
    }

    #[test]
    fn tile_letter_falls_back_to_a_dot() {
        assert_eq!(tile_letter(""), '·');
        assert_eq!(tile_letter("  x"), '·');
        assert_eq!(tile_letter("🚀 go"), '·');
    }

    #[test]
    fn amount_runs_from_start_to_target_with_ease_out() {
        assert_eq!(amount(0.0, 1.0, 0.0), 0.0);
        assert_eq!(amount(0.0, 1.0, ANIM_MS), 1.0);
        assert_eq!(amount(0.0, 1.0, 10_000.0), 1.0);
        assert!(amount(0.0, 1.0, ANIM_MS / 2.0) > 0.5);
    }

    #[test]
    fn a_reversal_starts_from_where_it_is() {
        assert_eq!(amount(0.4, 0.0, 0.0), 0.4);
        assert_eq!(amount(0.4, 0.0, ANIM_MS), 0.0);
    }

    #[test]
    fn width_is_linear_between_expanded_and_compact() {
        assert_eq!(width(300.0, 0.0), 300.0);
        assert_eq!(width(300.0, 1.0), WIDTH);
        assert_eq!(width(300.0, 0.5), (300.0 + WIDTH) / 2.0);
    }
}
