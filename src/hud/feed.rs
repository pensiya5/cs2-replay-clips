use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

use super::{
    FPS, Pixel, Top, dice,
    glyphs::{FEED_NAMES, FEED_ROWS, SUBJECT_ROWS},
    rounded, spread,
};

const WIDTH: usize = 430;
type Key = (usize, &'static str);

fn team_text([r, g, b]: Pixel, feed: bool) -> [bool; 2] {
    let blue = b - r > 36 && if feed { b > 153 && b - g > 27 } else { b > 132 };
    [blue, r - b > 48 && r > 170 && g > 126]
}

struct Glyph {
    rows: Vec<u128>,
    width: usize,
    pixels: u32,
}

fn glyphs() -> &'static [Glyph; 6] {
    static GLYPHS: OnceLock<[Glyph; 6]> = OnceLock::new();
    GLYPHS.get_or_init(|| {
        std::array::from_fn(|i| {
            let rows = if i < FEED_ROWS.len() {
                FEED_ROWS[i].to_vec()
            } else {
                SUBJECT_ROWS
                    .iter()
                    .map(|&row| {
                        (0..40).fold(0, |out, x| {
                            if row & (1 << x) != 0 {
                                out | (3 << (x * 2))
                            } else {
                                out
                            }
                        })
                    })
                    .collect()
            };
            let columns = rows.iter().fold(0, |out, &row| out | row);
            let start = columns.trailing_zeros();
            let width = (128 - columns.leading_zeros() - start) as usize;
            let first = rows.iter().position(|&row| row != 0).unwrap();
            let last = rows.iter().rposition(|&row| row != 0).unwrap();
            let rows: Vec<_> = rows[first..=last].iter().map(|&row| row >> start).collect();
            let pixels = rows.iter().map(|row| row.count_ones()).sum();
            Glyph {
                rows,
                width,
                pixels,
            }
        })
    })
}

pub(super) fn subject_kills(top: &Top, pitch: isize) -> HashSet<Key> {
    let scale = pitch as f64 / 58.0;
    let left = rounded(top.width as f64 * 0.62);
    let y0 = rounded(40.0 * scale);
    let mut edges = vec![None; top.height];
    for y in y0..top.height as isize {
        let mut first = 0;
        let mut last = 0;
        let mut n = 0;
        for x in left..top.width as isize {
            let [r, g, b] = top.pixel(y, x);
            if r > 105 && r - g > 64 && r - b > 64 {
                if n == 0 {
                    first = x;
                }
                last = x;
                n += 1;
            }
        }
        if n as f64 > 95.0 * scale
            && last as f64 > top.width as f64 - 36.0 * scale
            && n as f64 >= 0.82 * (last - first + 1) as f64
        {
            edges[y as usize] = Some((first, last));
        }
    }
    let sample = |y: isize, x: isize, row: isize, col: isize| {
        team_text(
            top.pixel(
                y + rounded(row as f64 * scale),
                x + rounded(col as f64 * scale),
            ),
            false,
        )
    };
    let feed_pixels = FEED_ROWS.map(|glyph| glyph.iter().map(|row| row.count_ones()).sum());
    let subject_pixels = SUBJECT_ROWS.iter().map(|row| row.count_ones()).sum();
    let mut detected = HashSet::new();
    for (y, edge) in edges.iter().enumerate() {
        let Some((x1, x2)) = *edge else {
            continue;
        };
        let y = y as isize;
        if !(y + rounded(27.0 * scale)..=y + rounded(32.0 * scale)).any(|row| {
            edges
                .get(row as usize)
                .and_then(|v| *v)
                .is_some_and(|(l, r)| {
                    (l - x1).abs() as f64 <= 7.0 * scale && (r - x2).abs() as f64 <= 4.0 * scale
                })
        }) {
            continue;
        }
        let victim: [[u128; 20]; 2] = std::array::from_fn(|side| {
            std::array::from_fn(|row| {
                (0..66).fold(0, |bits, col| {
                    bits | (u128::from(sample(y, x2, row as isize + 6, col - 73)[side]) << col)
                })
            })
        });
        let mut scores = [0.0_f64; 10];
        for side in 0..2 {
            for dy in 0..5 {
                for dx in 0..5 {
                    let rows: [u128; 16] =
                        std::array::from_fn(|row| (victim[side][dy + row] >> dx) & ((1 << 62) - 1));
                    for (name, glyph) in FEED_ROWS.iter().enumerate() {
                        let i = 5 * side + name;
                        scores[i] = scores[i].max(dice(&rows, glyph, feed_pixels[name]));
                    }
                }
            }
        }
        let mut order = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        let best = order[0];
        if scores[best] < 0.70 || scores[best] - scores[order[1]] < 0.08 {
            continue;
        }

        let killer: [[u128; 17]; 2] = std::array::from_fn(|side| {
            std::array::from_fn(|row| {
                (0..114).fold(0, |bits, col| {
                    bits | (u128::from(sample(y, x1, row as isize + 8, col + 7)[side]) << col)
                })
            })
        });
        let matched = killer.iter().any(|side| {
            (0..6).any(|dy| {
                (0..36).any(|dx| {
                    let rows: [u128; 12] = std::array::from_fn(|row| {
                        (0..40).fold(0, |bits, col| {
                            bits | (((side[dy + row] >> (dx + 2 * col)) & 1) << col)
                        })
                    });
                    dice(&rows, &SUBJECT_ROWS, subject_pixels) >= 0.71
                })
            })
        });
        if matched {
            detected.insert((best / 5, FEED_NAMES[best % 5]));
        }
    }
    detected
}

struct Corner {
    pixels: Vec<Pixel>,
    teams: Vec<[bool; 2]>,
    height: usize,
}

impl Corner {
    fn sample(top: &Top, scale: f64) -> Self {
        let height = rounded(top.height as f64 / scale) as usize;
        let mut pixels = Vec::with_capacity(height * WIDTH);
        let mut teams = Vec::with_capacity(height * WIDTH);
        let xs: [isize; WIDTH] =
            std::array::from_fn(|x| top.width as isize - rounded((WIDTH - x) as f64 * scale));
        for y in 0..height {
            let row = rounded(y as f64 * scale);
            for x in xs {
                let pixel = top.pixel(row, x);
                pixels.push(pixel);
                teams.push(team_text(pixel, true));
            }
        }
        Self {
            pixels,
            teams,
            height,
        }
    }

    fn bits(&self, side: usize, y: usize, x: usize, width: usize) -> u128 {
        self.teams[y * WIDTH + x..y * WIDTH + x + width]
            .iter()
            .enumerate()
            .fold(0, |out, (i, v)| out | (u128::from(v[side]) << i))
    }

    fn count(&self, side: usize, y: usize, x: usize, height: usize, width: usize) -> u32 {
        (y..(y + height).min(self.height))
            .map(|row| self.bits(side, row, x, width).count_ones())
            .sum()
    }

    fn victim(&self, y: usize) -> Option<(Key, usize, usize)> {
        let x0 = WIDTH - 102;
        let y0 = y - 3;
        let height = (self.height - y0).min(18);
        if height < 12 {
            return None;
        }
        if (0..2)
            .map(|side| self.count(side, y0, x0, height, 85))
            .sum::<u32>()
            < 50
        {
            return None;
        }
        let glyphs = glyphs();
        let mut scores = [0.0_f64; 12];
        let mut best = (0.0_f64, 0, 0, 0, 0);
        for side in 0..2 {
            for dy in 0..=height - 12 {
                for dx in 0..7 {
                    let rows: [u128; 12] =
                        std::array::from_fn(|row| self.bits(side, y0 + dy + row, x0 + dx, 79));
                    let pixels: u32 = rows.iter().map(|row| row.count_ones()).sum();
                    for (name, glyph) in glyphs.iter().enumerate() {
                        let overlap: u32 = glyph
                            .rows
                            .iter()
                            .zip(rows)
                            .map(|(a, b)| ((a << (76 - glyph.width)) & b).count_ones())
                            .sum();
                        let score = f64::from(2 * overlap) / f64::from(pixels + glyph.pixels);
                        let i = 6 * side + name;
                        scores[i] = scores[i].max(score);
                        if score > best.0 {
                            best = (score, side, dy, dx, name);
                        }
                    }
                }
            }
        }
        scores.sort_by(|a, b| b.total_cmp(a));
        let (score, side, dy, dx, name) = best;
        (score >= 0.66 && score - scores[1] >= 0.065).then_some((
            (side, FEED_NAMES[name]),
            y0 + dy,
            x0 + dx + 76 - glyphs[name].width,
        ))
    }

    fn killer(&self, y: usize, victim_x: usize) -> Option<&'static str> {
        if y < 9 {
            return None;
        }
        let below = (y + 14..(y + 19).min(self.height)).len();
        let n = 5 + below;
        let contrast: [f64; WIDTH - 6] = std::array::from_fn(|x| {
            let mut sums = [0_i16; 10];
            for (i, sum) in sums[..n].iter_mut().enumerate() {
                let row = if i < 5 { y - 9 + i } else { y + 14 + i - 5 };
                let a = self.pixels[row * WIDTH + x];
                let b = self.pixels[row * WIDTH + x + 6];
                *sum = (0..3).map(|c| (a[c] - b[c]).abs()).sum();
            }
            sums[..n].sort_unstable();
            f64::from(sums[(n - 1) / 2] + sums[n / 2]) / 6.0
        });
        let mut origins = [false; WIDTH];
        for (x, &value) in contrast.iter().enumerate().filter(|&(_, v)| *v > 10.0) {
            let _ = value;
            for offset in (9..17).chain(34..43) {
                if let Some(origin) = origins.get_mut(x + 3 + offset) {
                    *origin = true;
                }
            }
        }
        let mut bright = vec![0; victim_x + 1];
        for x in 0..victim_x {
            bright[x + 1] = bright[x]
                + (y - 5..(y + 15).min(self.height))
                    .filter(|&row| {
                        let rgb = self.pixels[row * WIDTH + x];
                        rgb.into_iter().all(|c| c > 156) && spread(rgb) < 48
                    })
                    .count();
        }
        let mut matches = Vec::new();
        for (name, glyph) in glyphs().iter().enumerate() {
            for x in (20..victim_x).filter(|&x| origins[x] && x + glyph.width + 5 < victim_x) {
                let end = x + glyph.width;
                if bright[(end + 18).min(victim_x)] - bright[end] < 6
                    || bright[victim_x] - bright[end] < 20
                {
                    continue;
                }
                for glyph_y in y - 1..=y + 1 {
                    if glyph_y + glyph.rows.len() > self.height {
                        continue;
                    }
                    for side in 0..2 {
                        let rows: Vec<_> = (glyph_y..glyph_y + glyph.rows.len())
                            .map(|row| self.bits(side, row, x, glyph.width))
                            .collect();
                        let score = dice(&rows, &glyph.rows, glyph.pixels);
                        if score < 0.64
                            || self.count(side, glyph_y, x - 3, 9, 3) > 5
                            || self.count(side, glyph_y, end + 1, 9, 3) > 5
                        {
                            continue;
                        }
                        let direct = contrast[x - 16..x - 10].iter().any(|&c| c > 10.0);
                        let with_icon = self.count(
                            side,
                            y,
                            x.saturating_sub(31),
                            9,
                            x - 4 - x.saturating_sub(31),
                        ) < 16
                            && contrast[x.saturating_sub(42)..x.saturating_sub(36)]
                                .iter()
                                .any(|&c| c > 10.0);
                        if direct || with_icon {
                            matches.push((score, name, x));
                        }
                    }
                }
            }
        }
        let cutoff = (matches.iter().map(|m| m.0).fold(0.0_f64, f64::max) - 0.075).max(0.66);
        let first = matches
            .iter()
            .filter(|m| m.0 >= cutoff)
            .map(|m| m.2)
            .min()?;
        let mut names = matches
            .into_iter()
            .filter(|m| m.0 >= cutoff && m.2 <= first + 6)
            .map(|m| m.1);
        let name = names.next()?;
        names.all(|i| i == name).then_some(FEED_NAMES[name])
    }
}

pub(super) struct FeedEntry {
    pub(super) first_seen: f64,
    pub(super) last_seen: f64,
    pub(super) killer: Option<&'static str>,
}

pub(super) struct KillFeed {
    scale: f64,
    pub(super) sample_gap: f64,
    pub(super) rows: HashMap<Key, FeedEntry>,
}

impl KillFeed {
    pub(super) fn new(pitch: isize) -> Self {
        Self {
            scale: pitch as f64 / 58.0,
            sample_gap: 1.25 / FPS,
            rows: HashMap::new(),
        }
    }

    pub(super) fn observe(&mut self, top: &Top, time: f64) {
        self.rows
            .retain(|_, row| time - row.last_seen <= 2.0 * self.sample_gap);
        if top.width < rounded(WIDTH as f64 * self.scale) as usize {
            return;
        }
        let corner = Corner::sample(top, self.scale);
        let mut victims: HashMap<Key, Vec<(usize, usize)>> = HashMap::new();
        for y in (86..corner.height.saturating_sub(11)).step_by(34) {
            if let Some((key, y, x)) = corner.victim(y) {
                victims.entry(key).or_default().push((y, x));
            }
        }
        for (key, positions) in victims {
            let previous = self.rows.get(&key);
            let first_seen = previous.map_or(time, |row| row.first_seen);
            let killer = if let [(y, x)] = positions.as_slice() {
                previous
                    .and_then(|row| row.killer)
                    .or_else(|| corner.killer(*y, *x))
            } else {
                None
            };
            self.rows.insert(
                key,
                FeedEntry {
                    first_seen,
                    last_seen: time,
                    killer,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn victim_search_handles_the_bottom_of_the_hud() {
        let height = 270;
        let corner = Corner {
            pixels: vec![[0; 3]; WIDTH * height],
            teams: vec![[true, false]; WIDTH * height],
            height,
        };
        // The candidate is within the feed, but its full 18-row patch isn't.
        assert!(corner.victim(256).is_none());
    }
}
