//! Latency sparkline drawn with `iced`'s canvas widget.

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, mouse};

/// A rolling latency history. `None` marks a probe round where the target did
/// not answer, which is drawn as a red band rather than a gap in the line.
pub struct Latency<'a> {
    pub samples: &'a [Option<f64>],
    /// Number of slots on the x-axis, so the line grows from the left instead
    /// of rescaling on every new sample.
    pub capacity: usize,
    pub label: &'a str,
}

const PADDING: f32 = 8.0;
const LEFT_GUTTER: f32 = 38.0;

impl<Message> canvas::Program<Message> for Latency<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();

        let plot = Rectangle {
            x: LEFT_GUTTER,
            y: PADDING,
            width: (bounds.width - LEFT_GUTTER - PADDING).max(1.0),
            height: (bounds.height - PADDING * 2.0 - 12.0).max(1.0),
        };

        frame.fill_rectangle(
            Point::new(plot.x, plot.y),
            Size::new(plot.width, plot.height),
            palette.background.weak.color,
        );

        let axis_color = Color {
            a: 0.35,
            ..palette.background.strong.color
        };
        let text_color = palette.background.base.text;

        // Round the scale up to something readable so the line does not jump
        // every time the peak moves a little.
        let peak = self
            .samples
            .iter()
            .filter_map(|sample| *sample)
            .fold(0.0_f64, f64::max);
        let scale_max = nice_ceiling(peak.max(20.0));

        for step in 0..=4 {
            let fraction = step as f32 / 4.0;
            let y = plot.y + plot.height * (1.0 - fraction);

            frame.stroke(
                &Path::line(Point::new(plot.x, y), Point::new(plot.x + plot.width, y)),
                Stroke::default().with_width(1.0).with_color(axis_color),
            );

            frame.fill_text(Text {
                content: format!("{:.0}", scale_max * fraction as f64),
                position: Point::new(plot.x - 6.0, y),
                color: text_color,
                size: 10.0.into(),
                align_x: iced::alignment::Horizontal::Right.into(),
                align_y: iced::alignment::Vertical::Center,
                ..Text::default()
            });
        }

        if self.samples.is_empty() {
            return vec![frame.into_geometry()];
        }

        let capacity = self.capacity.max(self.samples.len()).max(2);
        let step = plot.width / (capacity - 1) as f32;
        // Newest sample sits at the right edge.
        let offset = capacity.saturating_sub(self.samples.len());

        let x_at = |index: usize| plot.x + (offset + index) as f32 * step;
        let y_at = |latency: f64| {
            let fraction = (latency / scale_max).clamp(0.0, 1.0) as f32;

            plot.y + plot.height * (1.0 - fraction)
        };

        // Failures first, so the line is drawn over them.
        for (index, sample) in self.samples.iter().enumerate() {
            if sample.is_none() {
                frame.fill_rectangle(
                    Point::new(x_at(index) - step.max(1.0) / 2.0, plot.y),
                    Size::new(step.max(1.5), plot.height),
                    Color {
                        a: 0.45,
                        ..palette.danger.base.color
                    },
                );
            }
        }

        // One sub-path per unbroken run of successful probes.
        let line = Path::new(|builder| {
            let mut drawing = false;

            for (index, sample) in self.samples.iter().enumerate() {
                match sample {
                    Some(latency) => {
                        let point = Point::new(x_at(index), y_at(*latency));

                        if drawing {
                            builder.line_to(point);
                        } else {
                            builder.move_to(point);
                            drawing = true;
                        }
                    }
                    None => drawing = false,
                }
            }
        });

        frame.stroke(
            &line,
            Stroke::default()
                .with_width(1.6)
                .with_color(palette.primary.base.color),
        );

        frame.fill_text(Text {
            content: format!("{} · peak {peak:.0} ms", self.label),
            position: Point::new(plot.x, plot.y + plot.height + 4.0),
            color: Color {
                a: 0.7,
                ..text_color
            },
            size: 10.0.into(),
            ..Text::default()
        });

        vec![frame.into_geometry()]
    }
}

/// Rounds up to 1, 2 or 5 times a power of ten.
fn nice_ceiling(value: f64) -> f64 {
    if value <= 0.0 {
        return 1.0;
    }

    let magnitude = 10.0_f64.powf(value.log10().floor());
    let normalized = value / magnitude;

    let rounded = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };

    rounded * magnitude
}

#[cfg(test)]
mod tests {
    use super::nice_ceiling;

    #[test]
    fn scales_round_to_readable_numbers() {
        assert_eq!(nice_ceiling(20.0), 20.0);
        assert_eq!(nice_ceiling(23.0), 50.0);
        assert_eq!(nice_ceiling(140.0), 200.0);
        assert_eq!(nice_ceiling(0.0), 1.0);
    }
}
