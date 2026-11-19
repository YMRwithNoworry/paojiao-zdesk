use super::{Action, Canvas, Color, RectF};

/// Outline icons use a 16-DIP design grid shared by the Shell command bar and
/// menu body. Draw them at the monitor's scale, including disabled contrast.
pub(super) struct Icon<'a, 'b> {
    pub cv: &'a Canvas<'b>,
    pub rect: RectF,
    pub ink: Color,
    pub accent: Color,
    pub paper: Color,
}

impl Icon<'_, '_> {
    fn box_at(&self, x: f32, y: f32, w: f32, h: f32) -> RectF {
        let s = self.rect.w / 16.;
        RectF::new(self.rect.x + s * x, self.rect.y + s * y, s * w, s * h)
    }
    fn outline(&self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Color) {
        let s = self.rect.w / 16.;
        self.cv
            .stroke_round(self.box_at(x, y, w, h), s * r, color, s);
    }
    fn line(&self, points: &[(f32, f32)], color: Color) {
        let s = self.rect.w / 16.;
        for pair in points.windows(2) {
            let (x, y) = pair[0];
            let (u, v) = pair[1];
            self.cv.line(
                self.rect.x + x * s,
                self.rect.y + y * s,
                self.rect.x + u * s,
                self.rect.y + v * s,
                color,
                s,
            );
        }
    }
    pub fn toolbar(&self, action: Action) {
        let (ink, blue) = (self.ink, self.accent);
        match action {
            Action::Copy => {
                self.line(
                    &[
                        (5., 4.5),
                        (3., 4.5),
                        (1.5, 6.),
                        (1.5, 13.),
                        (3., 14.5),
                        (8., 14.5),
                        (9.5, 13.),
                    ],
                    ink,
                );
                self.outline(6., 1., 9., 11., 2., blue);
            }
            Action::Cut => {
                self.line(&[(3.5, 0.5), (10.5, 10.5)], ink);
                self.line(&[(12.5, 0.5), (5.5, 10.5)], ink);
                self.outline(1., 10., 6., 6., 3., blue);
                self.outline(9., 10., 6., 6., 3., blue);
            }
            Action::Rename => {
                self.line(
                    &[
                        (9., 2.5),
                        (2., 2.5),
                        (0.5, 4.),
                        (0.5, 12.),
                        (2., 13.5),
                        (9., 13.5),
                    ],
                    ink,
                );
                self.line(
                    &[
                        (12.5, 2.5),
                        (14., 2.5),
                        (15.5, 4.),
                        (15.5, 12.),
                        (14., 13.5),
                        (12.5, 13.5),
                    ],
                    ink,
                );
                self.line(&[(2.75, 11.), (5.35, 4.65), (8., 11.)], blue);
                self.line(&[(3.7, 8.5), (7.1, 8.5)], blue);
                self.line(&[(10.5, 0.5), (10.5, 15.5)], blue);
                self.line(&[(8., 0.5), (13., 0.5)], blue);
                self.line(&[(8., 15.5), (13., 15.5)], blue);
            }
            Action::Share => {
                self.line(
                    &[
                        (7., 2.5),
                        (3., 2.5),
                        (1.5, 4.),
                        (1.5, 13.),
                        (3., 14.5),
                        (12., 14.5),
                        (13.5, 13.),
                        (13.5, 11.),
                    ],
                    ink,
                );
                self.line(
                    &[
                        (10.5, 1.5),
                        (15.5, 6.5),
                        (10.5, 11.),
                        (10.5, 8.),
                        (8., 8.),
                        (5.5, 9.),
                        (4.5, 10.5),
                        (5., 7.),
                        (7., 5.),
                        (10.5, 4.5),
                        (10.5, 1.5),
                    ],
                    blue,
                );
            }
            Action::Delete => {
                self.line(
                    &[
                        (5.5, 2.5),
                        (6., 1.),
                        (7., 0.5),
                        (9., 0.5),
                        (10., 1.),
                        (10.5, 2.5),
                    ],
                    ink,
                );
                self.line(&[(0.5, 2.5), (15.5, 2.5)], ink);
                self.line(
                    &[
                        (2., 3.),
                        (3.5, 14.),
                        (4.5, 15.5),
                        (11.5, 15.5),
                        (12.5, 14.),
                        (14., 3.),
                    ],
                    ink,
                );
                self.line(&[(6.5, 6.), (6.5, 12.)], ink);
                self.line(&[(9.5, 6.), (9.5, 12.)], ink);
            }
        }
    }
    pub fn body(&self, verb: &str) -> bool {
        let (ink, blue) = (self.ink, self.accent);
        match verb {
            "newfolder" => {
                self.outline(0., 0., 15., 15., 7.5, ink);
                self.line(&[(4., 7.5), (11., 7.5)], blue);
                self.line(&[(7.5, 4.), (7.5, 11.)], blue);
            }
            "groupby" => {
                for top in [1.5, 9.5] {
                    self.line(
                        &[(4., top), (1.5, top), (1.5, top + 5.), (4., top + 5.)],
                        blue,
                    );
                    self.line(&[(5., top + 1.), (15., top + 1.)], ink);
                    self.line(&[(5., top + 4.), (15., top + 4.)], ink);
                }
            }
            "arrange" => {
                self.line(&[(0.5, 6.5), (4.5, 2.5), (8.5, 6.5)], ink);
                self.line(&[(4.5, 2.5), (4.5, 14.)], ink);
                self.line(&[(7.5, 9.5), (11.5, 13.5), (15.5, 9.5)], blue);
                self.line(&[(11.5, 2.), (11.5, 13.5)], blue);
            }
            "sharewith" => self.toolbar(Action::Share),
            "pintohomefile" => self.line(
                &[
                    (8., 0.5),
                    (10.4, 5.3),
                    (15.5, 6.4),
                    (11.6, 10.2),
                    (12.5, 15.1),
                    (8., 12.7),
                    (3.5, 15.1),
                    (4.4, 10.2),
                    (0.5, 6.4),
                    (5.6, 5.3),
                    (8., 0.5),
                ],
                blue,
            ),
            "openas" => {
                for y in [1., 6., 11.] {
                    self.outline(1., y, 4., 4., 1., ink);
                }
                self.line(&[(6., 2.5), (15., 2.5)], ink);
                self.line(&[(6., 7.5), (8., 7.5)], ink);
                self.cv
                    .round(self.box_at(7., 7., 9., 9.), self.rect.w * 4.5 / 16., blue);
                let white = self.paper;
                self.line(&[(10., 13.5), (13., 10.5), (10., 10.5)], white);
                self.line(&[(13., 10.5), (13., 13.)], white);
            }
            "copyaspath" => {
                self.outline(0., 2., 16., 12., 3., ink);
                self.line(&[(2.5, 4.5), (4.5, 11.5)], blue);
                self.line(&[(5.5, 4.5), (7.5, 11.5)], blue);
                self.line(&[(9.5, 11.5), (10.5, 10.5)], blue);
                self.line(&[(12., 10.5), (13., 11.5)], blue);
            }
            "zsdesk.more" => {
                self.line(
                    &[
                        (7., 1.5),
                        (3., 1.5),
                        (1.5, 3.5),
                        (1.5, 12.5),
                        (3., 14.5),
                        (12.5, 14.5),
                        (14.5, 12.5),
                        (14.5, 9.),
                    ],
                    ink,
                );
                self.line(&[(1.5, 8.5), (5., 8.5), (7.5, 10.5), (7.5, 14.5)], ink);
                self.line(&[(9., 1.5), (14.5, 1.5), (14.5, 7.)], blue);
                self.line(&[(14., 2.), (8.5, 7.5)], blue);
            }
            "properties" => self.cv.glyph("\u{E90F}", self.rect, self.rect.w, ink),
            _ => return false,
        }
        true
    }
}
