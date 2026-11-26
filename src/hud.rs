//! Identify CS2 replay deaths from the two team rosters and corroborate the kill feed.
mod feed;
mod glyphs;
mod roster;

use std::{collections::HashMap, path::Path};

use crate::video::{frames::render_rgb24, webm::Video};
use serde_json::{Value, json};

use feed::{KillFeed, subject_kills};
use roster::{Roster, calibrate};

const FPS: f64 = 2.0;
const CALIBRATION_SECONDS: f64 = 20.0;

type Pixel = [i16; 3];

struct Top {
    rgb: Vec<u8>,
    width: usize,
    height: usize,
    full_height: usize,
}

impl Top {
    fn pixel(&self, y: isize, x: isize) -> Pixel {
        let y = y.clamp(0, self.height as isize - 1) as usize;
        let x = x.clamp(0, self.width as isize - 1) as usize;
        let index = (y * self.width + x) * 3;
        std::array::from_fn(|channel| i16::from(self.rgb[index + channel]))
    }
}

fn rounded(value: f64) -> isize {
    value.round_ties_even() as isize
}

fn delta(a: Pixel, b: Pixel) -> i16 {
    (0..3).map(|i| (a[i] - b[i]).abs()).max().unwrap_or(0)
}

fn spread(pixel: Pixel) -> i16 {
    pixel.into_iter().max().unwrap() - pixel.into_iter().min().unwrap()
}

fn white(pixel: Pixel) -> bool {
    pixel.into_iter().all(|c| c > 192) && spread(pixel) < 45
}

fn dice(rows: &[u128], expected: &[u128], pixels: u32) -> f64 {
    let overlap: u32 = rows
        .iter()
        .zip(expected)
        .map(|(a, b)| (a & b).count_ones())
        .sum();
    let total = pixels + rows.iter().map(|r| r.count_ones()).sum::<u32>();
    if total == 0 {
        0.0
    } else {
        f64::from(2 * overlap) / f64::from(total)
    }
}

#[derive(Clone, Copy, Default)]
struct Player {
    alive: bool,
    seen: u8,
    missing_since: Option<f64>,
    evidence: f64,
    name: Option<&'static str>,
    next_name_check: f64,
}

struct Tracker {
    roster: Roster,
    players: [Player; 10],
    last_clock: Option<(f64, i32)>,
    highlighted: HashMap<(usize, &'static str), f64>,
    feed: KillFeed,
}

impl Tracker {
    fn new(roster: Roster) -> Self {
        Self {
            feed: KillFeed::new(roster.pitch()),
            roster,
            players: [Player::default(); 10],
            last_clock: None,
            highlighted: HashMap::new(),
        }
    }

    fn reset(&mut self) {
        self.players.fill(Player::default());
        self.highlighted.clear();
        self.feed.rows.clear();
    }

    fn observe(&mut self, time: f64, top: &Top, events: &mut Vec<(f64, Value)>) {
        let (hp, border) = self.roster.observe(top);
        let visible: [bool; 10] = std::array::from_fn(|i| hp[i] || border[i]);
        if !visible.into_iter().any(|v| v) {
            self.reset();
            self.last_clock = None;
            return;
        }

        if let Some(clock) = self.roster.clock(top) {
            if let Some((last_time, last_clock)) = self.last_clock {
                let elapsed = time - last_time;
                if elapsed <= 2.0
                    && !(-2.0..=elapsed + 2.0).contains(&f64::from(last_clock - clock))
                {
                    self.reset();
                }
            }
            self.last_clock = Some((time, clock));
        } else if self
            .last_clock
            .is_some_and(|(last, _)| time - last <= 1.25 / FPS)
        {
            let dropped: Vec<_> = self
                .players
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    (p.alive && p.missing_since.is_none() && !visible[i]).then_some(i)
                })
                .collect();
            if dropped.len() >= 3 && dropped[0] < 5 && dropped[dropped.len() - 1] >= 5 {
                for i in dropped {
                    self.players[i] = Player::default();
                }
            }
        }
        if (0..10)
            .filter(|&i| hp[i] && border[i] && !self.players[i].alive)
            .count()
            >= 2
        {
            self.players.fill(Player::default());
            self.feed.rows.clear();
        }
        self.highlighted = subject_kills(top, self.roster.pitch())
            .into_iter()
            .map(|key| (key, self.highlighted.get(&key).copied().unwrap_or(time)))
            .collect();
        self.feed.observe(top, time);

        for (slot, player) in self.players.iter_mut().enumerate() {
            if visible[slot] {
                player.missing_since = None;
                if !player.alive {
                    player.seen += 1;
                    player.alive = player.seen >= 2 || (hp[slot] && border[slot]);
                }
                if player.alive {
                    player.evidence = if hp[slot] && border[slot] { 0.96 } else { 0.85 };
                    if time >= player.next_name_check {
                        player.name = self.roster.name(top, slot).or(player.name);
                        player.next_name_check = time + 1.5;
                    }
                }
                continue;
            }
            player.seen = 0;
            if !player.alive {
                continue;
            }
            let Some(missing) = player.missing_since else {
                player.missing_since = Some(time);
                continue;
            };
            if time - missing < 0.80 / FPS {
                continue;
            }
            let side = if slot < 5 { "left" } else { "right" };
            let mut event = json!({
                "t": (missing * 1000.0).round_ties_even() / 1000.0,
                "victim_slot": format!("{side}_{}", slot % 5 + 1),
                "confidence": player.evidence,
                "killer": "не определён",
            });
            if let Some(name) = player.name {
                event["victim"] = json!(name);
                let key = (slot / 5, name);
                let earliest = missing - 1.0 / FPS - 1e-6;
                let verified = self
                    .highlighted
                    .get(&key)
                    .is_some_and(|&seen| seen >= earliest);
                if let Some(killer) = self
                    .feed
                    .rows
                    .get(&key)
                    .filter(|row| {
                        row.first_seen >= earliest && time - row.last_seen <= self.feed.sample_gap
                    })
                    .and_then(|row| row.killer)
                {
                    if !verified || killer == "The Subject" {
                        event["killer"] = json!(killer);
                    }
                } else if verified {
                    event["killer"] = json!("The Subject");
                }
            }
            events.push((missing, event));
            *player = Player::default();
        }
    }
}

pub fn detect(path: &Path, start: f64, end: f64) -> Result<Vec<Value>, String> {
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        return Err("HUD: нужны конечные границы 0 ≤ start < end".into());
    }
    let sample_start = (start - 1.0).max(0.0);
    let sample_end = end + 1.20;
    let mut video = Video::open(path, 8).map_err(|e| e.to_string())?;
    let first = video
        .next_picture()
        .map_err(|e| e.to_string())?
        .ok_or("Нет кадров видео")?;
    let origin = first.pts_ns();
    let target = origin
        .checked_add((sample_start * 1e9) as i64)
        .ok_or("HUD: неверное начало")?;
    let mut first = Some(first);
    if let Ok(Some((positioned, picture))) = Video::open_at_keyframe(path, 8, target) {
        video = positioned;
        first = Some(picture);
    }
    let mut next = sample_start;
    let mut next_calibration = sample_start;
    let mut pending = Vec::new();
    let mut tracker: Option<Tracker> = None;
    let mut events = Vec::new();
    let mut rgb = Vec::new();
    loop {
        let picture = match first.take() {
            Some(picture) => picture,
            None => match video.next_picture().map_err(|e| e.to_string())? {
                Some(picture) => picture,
                None => break,
            },
        };
        let time = (picture.pts_ns() - origin) as f64 / 1e9;
        if time > sample_end {
            break;
        }
        if time + 1e-6 < next {
            continue;
        }
        next = sample_start + (((time + 1e-6 - sample_start) * FPS).floor() + 1.0) / FPS;
        render_rgb24(&picture, &mut rgb).map_err(|e| e.to_string())?;
        let (width, full_height) = picture.dimensions();
        let height = full_height.div_ceil(4);
        let top = Top {
            rgb: rgb[..width * height * 3].to_vec(),
            width,
            height,
            full_height,
        };
        if let Some(tracker) = &mut tracker {
            tracker.observe(time, &top, &mut events);
            continue;
        }
        pending.push((time, top));
        if time >= next_calibration {
            let top = &pending.last().unwrap().1;
            if let Some(layout) = calibrate(top) {
                let mut roster = Roster::new(layout);
                roster.observe(top);
                let mut calibrated = Tracker::new(roster);
                for (time, top) in pending.drain(..) {
                    calibrated.observe(time, &top, &mut events);
                }
                tracker = Some(calibrated);
                continue;
            }
            next_calibration = time + 1.0;
        }
        if time >= sample_start + CALIBRATION_SECONDS {
            break;
        }
    }
    if tracker.is_none() {
        return Err("Не удалось найти две панели игроков HUD в первые 20 секунд".into());
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(events
        .into_iter()
        .filter(|(time, _)| (start..=end).contains(time))
        .map(|(_, event)| event)
        .collect())
}
