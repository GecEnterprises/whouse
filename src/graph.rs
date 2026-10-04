//! Braille area graph in the style of btop: every cell is 2 samples wide and 4
//! dots tall, newest sample at the right edge, colour follows height.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

pub type Stops = [(u8, u8, u8); 3];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grow {
    /// Anchored at the bottom edge, grows upward.
    Up,
    /// Anchored at the top edge, grows downward (mirror image).
    Down,
}

pub struct Graph<'a> {
    pub data: &'a [f64],
    pub max: f64,
    pub grow: Grow,
    pub stops: Stops,
}

/// Dot bits per column, ordered from the anchored edge inwards.
const UP: [[u32; 4]; 2] = [[0x40, 0x04, 0x02, 0x01], [0x80, 0x20, 0x10, 0x08]];
const DOWN: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

fn cell_mask(grow: Grow, col: usize, dots: usize, cell_row: usize) -> u32 {
    let filled = dots.saturating_sub(cell_row * 4).min(4);
    let bits = if grow == Grow::Up {
        &UP[col]
    } else {
        &DOWN[col]
    };
    bits[..filled].iter().fold(0, |m, b| m | b)
}

pub fn gradient(stops: Stops, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (a, b, u) = if t < 0.5 {
        (stops[0], stops[1], t * 2.0)
    } else {
        (stops[1], stops[2], (t - 0.5) * 2.0)
    };
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * u).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

impl Widget for Graph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.max <= 0.0 {
            return;
        }
        let samples = area.width as usize * 2;
        let total_dots = area.height as usize * 4;
        let visible = &self.data[self.data.len().saturating_sub(samples)..];
        let blank = samples - visible.len();
        let dots = |i: usize| -> usize {
            let Some(&v) = i.checked_sub(blank).and_then(|j| visible.get(j)) else {
                return 0;
            };
            if v <= 0.0 {
                return 0;
            }
            // Anything non-zero gets at least one dot so trickles stay visible.
            ((v / self.max * total_dots as f64).round() as usize).clamp(1, total_dots)
        };

        for cx in 0..area.width as usize {
            let heights = [dots(cx * 2), dots(cx * 2 + 1)];
            for row in 0..area.height as usize {
                let mask = cell_mask(self.grow, 0, heights[0], row)
                    | cell_mask(self.grow, 1, heights[1], row);
                if mask == 0 {
                    continue;
                }
                let y = match self.grow {
                    Grow::Up => area.bottom() - 1 - row as u16,
                    Grow::Down => area.top() + row as u16,
                };
                let t = (row * 4 + 2) as f64 / total_dots as f64;
                if let Some(ch) = char::from_u32(0x2800 + mask) {
                    buf[(area.x + cx as u16, y)]
                        .set_char(ch)
                        .set_fg(gradient(self.stops, t));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOPS: Stops = [(0, 0, 0), (100, 100, 100), (200, 200, 200)];

    fn render(data: &[f64], max: f64, grow: Grow, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        Graph {
            data,
            max,
            grow,
            stops: STOPS,
        }
        .render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    #[test]
    fn full_and_half_columns() {
        assert_eq!(render(&[10.0, 0.0], 10.0, Grow::Up, 1, 1), ["⡇"]);
        assert_eq!(render(&[0.0, 10.0], 10.0, Grow::Up, 1, 1), ["⢸"]);
        assert_eq!(render(&[5.0, 5.0], 10.0, Grow::Up, 1, 1), ["⣤"]);
        assert_eq!(render(&[5.0, 5.0], 10.0, Grow::Down, 1, 1), ["⠛"]);
    }

    #[test]
    fn newest_sample_is_right_aligned_and_old_ones_scroll_off() {
        // Two cells = 4 sample slots; three samples leave the first slot blank.
        assert_eq!(render(&[10.0, 10.0, 10.0], 10.0, Grow::Up, 2, 1), ["⢸⣿"]);
        // More samples than slots: only the newest four are drawn.
        assert_eq!(
            render(&[10.0, 0.0, 0.0, 0.0, 0.0, 10.0], 10.0, Grow::Up, 2, 1),
            [" ⢸"]
        );
    }

    #[test]
    fn tiny_values_still_draw_a_dot_and_tall_values_stack_cells() {
        assert_eq!(render(&[0.001, 0.0], 10.0, Grow::Up, 1, 1), ["⡀"]);
        assert_eq!(render(&[10.0, 10.0], 10.0, Grow::Up, 1, 2), ["⣿", "⣿"]);
        assert_eq!(render(&[5.0, 5.0], 10.0, Grow::Up, 1, 2), [" ", "⣿"]);
    }

    #[test]
    fn gradient_hits_its_stops() {
        assert_eq!(gradient(STOPS, 0.0), Color::Rgb(0, 0, 0));
        assert_eq!(gradient(STOPS, 0.5), Color::Rgb(100, 100, 100));
        assert_eq!(gradient(STOPS, 1.0), Color::Rgb(200, 200, 200));
        assert_eq!(gradient(STOPS, 0.25), Color::Rgb(50, 50, 50));
    }
}
