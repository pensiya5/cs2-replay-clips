use super::{
    Pixel, Top, delta, dice,
    glyphs::{CLOCK_ROWS, NAME_ROWS, NAMES},
    rounded, spread, white,
};

const SLOT_OFFSETS: [f64; 10] = [
    -5.67, -4.67, -3.67, -2.67, -1.67, 0.78, 1.78, 2.78, 3.78, 4.78,
];

#[derive(Clone, Copy)]
pub(super) struct Layout {
    x: [isize; 10],
    y: isize,
    pitch: isize,
}

struct Grid {
    score: f64,
    matched: usize,
    x: [isize; 10],
    pitch: isize,
}

fn fit(xs: &[isize], height: f64, width: f64) -> Option<Grid> {
    let mut best: Option<Grid> = None;
    for (i, &left) in xs.iter().enumerate() {
        for &right in &xs[i + 1..] {
            let pitch = right - left;
            if !(height * 0.030..=height * 0.081).contains(&(pitch as f64)) {
                continue;
            }
            for slot in [0, 1, 2, 3, 5, 6, 7, 8] {
                let origin = left as f64 - pitch as f64 * SLOT_OFFSETS[slot];
                if (origin - width / 2.0).abs() > width * 0.075 {
                    continue;
                }
                let mut x = [0; 10];
                let mut team_matches = [0, 0];
                let mut penalty = 0.0;
                for (i, offset) in SLOT_OFFSETS.iter().enumerate() {
                    let target = origin + pitch as f64 * offset;
                    let &nearest = xs
                        .iter()
                        .min_by(|&&a, &&b| {
                            (a as f64 - target)
                                .abs()
                                .total_cmp(&(b as f64 - target).abs())
                        })
                        .unwrap();
                    let error = (target - nearest as f64).abs();
                    penalty += error.min(3.0);
                    x[i] = if error <= (height * 0.002).max(2.0) {
                        team_matches[i / 5] += 1;
                        nearest
                    } else {
                        rounded(target)
                    };
                }
                let matched = if team_matches.contains(&0) {
                    0
                } else {
                    team_matches.iter().sum()
                };
                let score = matched as f64 - 0.01 * penalty;
                if best.as_ref().is_none_or(|grid| score > grid.score) {
                    best = Some(Grid {
                        score,
                        matched,
                        x,
                        pitch,
                    });
                }
            }
        }
    }
    best.filter(|grid| grid.matched >= 4)
}

pub(super) fn calibrate(top: &Top) -> Option<Layout> {
    let height = top.full_height as f64;
    let width = top.width as f64;
    let y_start = (height * 0.024) as isize;
    let y_end = (height * 0.19) as isize;
    let x_start = (width * 0.16) as isize;
    let x_end = (width * 0.84) as isize;
    let run = rounded(height * 0.009).max(5);
    let mut best_quality = (0, 0);
    let mut best = None;
    let mut xs = Vec::new();
    for y in y_start..y_end {
        xs.clear();
        let mut beginning = x_start;
        let mut consecutive = 0;
        for x in x_start..x_end {
            if white(top.pixel(y, x)) {
                if consecutive == 0 {
                    beginning = x;
                }
                consecutive += 1;
                if consecutive == run {
                    xs.push(beginning);
                }
            } else {
                consecutive = 0;
            }
        }
        if xs.len() < 4 {
            continue;
        }
        let Some(grid) = fit(&xs, height, width) else {
            continue;
        };
        if grid.x[0] <= 0 || grid.x[9] + grid.pitch >= top.width as isize {
            continue;
        }
        for adjustment in [0, -1, 1] {
            let layout = Layout {
                x: grid.x,
                y: y + adjustment,
                pitch: grid.pitch,
            };
            let mut roster = Roster::new(layout);
            let (hp, border) = roster.observe(top);
            let quality = (
                hp.into_iter().filter(|&v| v).count(),
                hp.into_iter()
                    .zip(border)
                    .filter(|&(hp, border)| hp && border)
                    .count(),
            );
            if quality <= best_quality
                || (quality.0 < 6
                    && (quality.0 < 4 || quality.1 < 3 || roster.clock(top).is_none()))
            {
                continue;
            }
            best_quality = quality;
            best = Some(layout);
            if quality == (10, 10) {
                return best;
            }
        }
    }
    best.or_else(|| {
        let scale = height / 1080.0;
        let pitch = rounded(58.0 * scale);
        let x = SLOT_OFFSETS.map(|offset| rounded(width / 2.0 + 58.0 * scale * offset));
        if pitch == 0 || x[0] <= 0 || x[9] + pitch >= top.width as isize {
            return None;
        }
        let layout = Layout {
            x,
            y: rounded(74.0 * scale),
            pitch,
        };
        let mut roster = Roster::new(layout);
        let (hp, border) = roster.observe(top);
        (hp.into_iter()
            .zip(border)
            .filter(|&(hp, border)| hp || border)
            .count()
            >= 2
            && (0..10)
                .filter(|&slot| roster.name(top, slot).is_some())
                .count()
                >= 6)
            .then_some(layout)
    })
}

pub(super) struct Roster {
    layout: Layout,
    hp_x: [isize; 13],
    hp_y: [isize; 4],
    border_x: [isize; 13],
    border_y: [isize; 8],
    side_x: [isize; 6],
    side_y: [isize; 11],
    name_x: [isize; 56],
    name_y: [isize; 13],
    clock_x: [isize; 50],
    clock_y: [isize; 23],
    palette: [Pixel; 10],
}

fn median<const N: usize>(pixels: [Pixel; N]) -> Pixel {
    std::array::from_fn(|i| {
        let mut channel = pixels.map(|p| p[i]);
        channel.sort_unstable();
        channel[N / 2]
    })
}

impl Roster {
    pub(super) fn new(layout: Layout) -> Self {
        let scale = |v| rounded(v * layout.pitch as f64 / 58.0);
        let y = |v| layout.y + scale(v);
        let center =
            rounded((layout.x[4] + layout.x[5]) as f64 / 2.0 + layout.pitch as f64 * 0.448);
        Self {
            layout,
            hp_x: std::array::from_fn(|i| scale(i as f64 + 2.0)),
            hp_y: [0.0, 1.0, 12.0, 13.0].map(y),
            border_x: std::array::from_fn(|i| scale(8.0 + 3.0 * i as f64)),
            border_y: std::array::from_fn(|i| y(i as f64 - 59.0)),
            side_x: [-1.0, 0.0, 1.0, 49.0, 50.0, 51.0].map(scale),
            side_y: std::array::from_fn(|i| y(-45.0 + 2.9 * i as f64)),
            name_x: std::array::from_fn(|i| scale(i as f64 - 2.0)),
            name_y: std::array::from_fn(|i| y(i as f64 - 69.0)),
            clock_x: std::array::from_fn(|i| center + scale(i as f64 - 25.0)),
            clock_y: std::array::from_fn(|i| y(i as f64 - 66.0)),
            palette: [[-255; 3]; 10],
        }
    }

    pub(super) fn pitch(&self) -> isize {
        self.layout.pitch
    }

    pub(super) fn observe(&mut self, top: &Top) -> ([bool; 10], [bool; 10]) {
        let mut hp = [false; 10];
        let mut outlines = [[None; 6]; 10];
        for (slot, left) in self.layout.x.iter().enumerate() {
            let bands = self.hp_y.map(|y| {
                self.hp_x
                    .iter()
                    .filter(|&&x| white(top.pixel(y, left + x)))
                    .count()
            });
            hp[slot] = bands[0].max(bands[1]) >= 10 && bands[2].max(bands[3]) >= 8;
            let lines = self
                .border_y
                .map(|y| self.border_x.map(|x| top.pixel(y, left + x)));
            let colors = lines.map(median);
            let side = self
                .side_x
                .map(|x| median(self.side_y.map(|y| top.pixel(y, left + x))));
            for i in 2..8 {
                let color = colors[i];
                if spread(color) > 48
                    && delta(color, colors[i - 2]) > 35
                    && side[..3].iter().any(|&p| delta(color, p) < 35)
                    && side[3..].iter().any(|&p| delta(color, p) < 35)
                    && lines[i].iter().filter(|&&p| delta(color, p) < 30).count() >= 12
                {
                    outlines[slot][i - 2] = Some(color);
                }
            }
            if hp[slot]
                && let Some(color) = outlines[slot].iter().flatten().next()
            {
                self.palette[slot] = *color;
            }
        }
        let border = outlines.map(|colors| {
            colors
                .iter()
                .flatten()
                .any(|&color| self.palette.iter().any(|&p| delta(color, p) <= 35))
        });
        (hp, border)
    }

    pub(super) fn clock(&self, top: &Top) -> Option<i32> {
        if self.clock_y[0] < 0 {
            return None;
        }
        let rows = self.clock_y.map(|y| {
            self.clock_x
                .iter()
                .enumerate()
                .fold(0_u64, |mask, (i, &x)| {
                    mask | (u64::from(white(top.pixel(y, x))) << i)
                })
        });
        for y in [9, 17] {
            if rows[y..y + 4]
                .iter()
                .map(|row| ((row >> 16) & 31).count_ones())
                .sum::<u32>()
                < 4
            {
                return None;
            }
        }
        let mut digits = [0; 3];
        for (digit, (start, end)) in digits.iter_mut().zip([(0, 16), (21, 36), (36, 50)]) {
            let part = rows.map(|row| (row >> start) & ((1 << (end - start)) - 1));
            if part.iter().map(|row| row.count_ones()).sum::<u32>() < 36 {
                return None;
            }
            let y0 = part.iter().position(|&row| row != 0)?;
            let y1 = part.iter().rposition(|&row| row != 0)?;
            let columns = part.iter().fold(0, |out, row| out | *row);
            let x0 = columns.trailing_zeros() as usize;
            let x1 = 63 - columns.leading_zeros() as usize;
            if !(15..20).contains(&(y1 - y0)) || !(8..12).contains(&(x1 - x0)) {
                return None;
            }
            let normalized: [u128; 18] = std::array::from_fn(|row| {
                let y = rounded(y0 as f64 + row as f64 * (y1 - y0) as f64 / 17.0) as usize;
                (0..10).fold(0, |mask, column| {
                    let x = rounded(x0 as f64 + column as f64 * (x1 - x0) as f64 / 9.0) as usize;
                    mask | (u128::from((part[y] >> x) & 1) << column)
                })
            });
            let mut errors = CLOCK_ROWS
                .map(|glyph| {
                    glyph
                        .iter()
                        .zip(normalized)
                        .map(|(a, b)| (a ^ b).count_ones())
                        .sum::<u32>()
                })
                .into_iter()
                .enumerate()
                .collect::<Vec<_>>();
            errors.sort_unstable_by_key(|&(digit, error)| (error, digit));
            if errors[0].1 > 25 || errors[1].1 - errors[0].1 < 7 {
                return None;
            }
            *digit = errors[0].0 as i32;
        }
        (digits[1] < 6).then_some(digits[0] * 60 + digits[1] * 10 + digits[2])
    }

    pub(super) fn name(&self, top: &Top, slot: usize) -> Option<&'static str> {
        let left = self.layout.x[slot];
        let patch = self.name_y.map(|y| {
            let levels = self
                .name_x
                .map(|x| top.pixel(y, left + x).into_iter().sum::<i16>());
            let mut sorted = levels;
            sorted.sort_unstable();
            levels
                .into_iter()
                .enumerate()
                .fold(0_u128, |mask, (i, level)| {
                    let bright = level > 261 && level * 2 > sorted[27] + sorted[28] + 138;
                    mask | (u128::from(bright) << i)
                })
        });
        let mut scores = [0.0_f64; 6];
        for y in 0..3 {
            for x in 0..5 {
                let crop: [u128; 11] =
                    std::array::from_fn(|row| (patch[y + row] >> x) & ((1 << 52) - 1));
                for (score, glyph) in scores.iter_mut().zip(NAME_ROWS) {
                    *score = score.max(dice(
                        &crop,
                        &glyph,
                        glyph.iter().map(|r| r.count_ones()).sum(),
                    ));
                }
            }
        }
        let mut order = [0, 1, 2, 3, 4, 5];
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        (scores[order[0]] >= 0.72 && scores[order[0]] - scores[order[1]] >= 0.10)
            .then_some(NAMES[order[0]])
    }
}
