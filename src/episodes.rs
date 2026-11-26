use serde_json::{Map, Value, json};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const PRE_ROLL: i64 = 10 * NANOS_PER_SECOND;
const POST_ROLL: i64 = 2 * NANOS_PER_SECOND;
const MERGE_GAP: i64 = 5 * NANOS_PER_SECOND;
const MIN_WINDOW: i64 = PRE_ROLL + POST_ROLL;
const MAX_WINDOW: i64 = 20 * NANOS_PER_SECOND;

struct Episode {
    first: Map<String, Value>,
    deaths: Vec<Value>,
    left: i64,
    right: i64,
    last_death: i64,
}

fn nanoseconds(seconds: f64) -> Option<i64> {
    // Fixed-point times keep threshold comparisons exact and retain fractional selection bounds.
    let ticks = (seconds * NANOS_PER_SECOND as f64).round();
    // Keep headroom for subtraction and roll adjustments on validated timestamps.
    (ticks.abs() < (i64::MAX / 2 - MAX_WINDOW) as f64).then_some(ticks as i64)
}

/// Group deaths selected for the given video range and compute their clip windows.
pub fn plan(events: Vec<Value>, start: f64, end: f64) -> Result<Vec<Value>, String> {
    let range_start = nanoseconds(start).ok_or("Некорректное начало выбранного диапазона")?;
    let range_end = nanoseconds(end).ok_or("Некорректный конец выбранного диапазона")?;
    if start > end {
        return Err("Конец выбранного диапазона раньше начала".into());
    }

    let mut timed_events = Vec::with_capacity(events.len());
    for (index, value) in events.into_iter().enumerate() {
        let Value::Object(event) = value else {
            return Err(format!("Событие смерти {} не является объектом", index + 1));
        };
        let time = event
            .get("t")
            .and_then(Value::as_f64)
            .and_then(nanoseconds)
            .ok_or_else(|| {
                format!(
                    "Некорректная отметка времени в событии смерти {}",
                    index + 1
                )
            })?;
        timed_events.push((time, event));
    }
    timed_events.sort_by_key(|(time, _)| *time);

    let mut episodes: Vec<Episode> = Vec::new();
    for (time, event) in timed_events {
        let right = range_end.min(time + POST_ROLL);
        let current = episodes.last_mut().filter(|episode| {
            time - episode.last_death <= MERGE_GAP && right - episode.left <= MAX_WINDOW
        });
        if let Some(episode) = current {
            episode
                .first
                .entry("victims")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or("Поле victims должно быть массивом")?
                .push(event.get("victim").cloned().unwrap_or(Value::Null));
            episode.deaths.push(Value::Object(event));
            episode.right = right;
            episode.last_death = time;
        } else {
            episodes.push(Episode {
                first: event.clone(),
                deaths: vec![Value::Object(event)],
                left: range_start.max(time - PRE_ROLL),
                right,
                last_death: time,
            });
        }
    }

    let mut result = Vec::with_capacity(episodes.len());
    for mut episode in episodes {
        let (mut left, mut right) = (episode.left, episode.right);
        if right - left < MIN_WINDOW {
            // Borrow context from the other side when the selection bounds cut a window short.
            right = range_end.min(left + MIN_WINDOW);
            left = range_start.max(right - MIN_WINDOW);
        }
        episode
            .first
            .insert("deaths".into(), Value::Array(episode.deaths));
        episode.first.insert(
            "window".into(),
            json!([
                left as f64 / NANOS_PER_SECOND as f64,
                right as f64 / NANOS_PER_SECOND as f64,
            ]),
        );
        result.push(Value::Object(episode.first));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::plan;
    use serde_json::{Value, json};

    fn deaths(times: &[f64]) -> Vec<Value> {
        times.iter().map(|time| json!({"t": time})).collect()
    }

    #[test]
    fn sorts_deaths_and_preserves_event_metadata() {
        let first = json!({"t": 21.2, "victim": "one", "killer": "other", "victim_slot": 3});
        let second = json!({"t": 24.1, "victim": "two", "extra": {"flag": true}});
        let third = json!({"t": 25.5, "victim_slot": 4});
        let fourth = json!({"t": 31.0, "victim": "four"});
        let episodes = plan(
            vec![second.clone(), fourth.clone(), third.clone(), first.clone()],
            0.0,
            60.0,
        )
        .unwrap();

        assert_eq!(
            episodes,
            vec![
                json!({
                    "t": 21.2, "victim": "one", "killer": "other", "victim_slot": 3,
                    "victims": ["two", null], "deaths": [first, second, third],
                    "window": [11.2, 27.5],
                }),
                json!({"t": 31.0, "victim": "four", "deaths": [fourth], "window": [21.0, 33.0]}),
            ],
        );
    }

    #[test]
    fn splits_at_more_than_five_seconds_between_deaths() {
        let episodes = plan(deaths(&[11.004, 16.004, 21.005]), 0.0, 60.0).unwrap();

        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0]["deaths"], json!(deaths(&[11.004, 16.004])));
        assert_eq!(episodes[0]["window"], json!([1.004, 18.004]));
        assert_eq!(episodes[1]["deaths"], json!(deaths(&[21.005])));
    }

    #[test]
    fn includes_a_twenty_second_window_and_splits_above_it() {
        let episodes = plan(deaths(&[30.002, 34.002, 38.002, 38.003]), 0.0, 80.0).unwrap();

        assert_eq!(episodes.len(), 2);
        assert_eq!(
            episodes[0]["deaths"],
            json!(deaths(&[30.002, 34.002, 38.002]))
        );
        assert_eq!(episodes[0]["window"], json!([20.002, 40.002]));
        assert_eq!(episodes[1]["window"], json!([28.003, 40.003]));
    }

    #[test]
    fn measures_the_cap_against_the_clamped_window() {
        let episodes = plan(deaths(&[86.0, 91.0, 95.0]), 0.0, 95.5).unwrap();

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0]["window"], json!([76.0, 95.5]));
    }

    #[test]
    fn extends_windows_to_twelve_seconds_at_fractional_boundaries() {
        let episodes = plan(deaths(&[10.001, 40.0]), 10.000_125, 40.000_275).unwrap();

        assert_eq!(episodes[0]["window"], json!([10.000_125, 22.000_125]));
        assert_eq!(episodes[1]["window"], json!([28.000_275, 40.000_275]));
    }

    #[test]
    fn uses_the_entire_selection_if_it_is_shorter_than_twelve_seconds() {
        let episodes = plan(deaths(&[10.0]), 9.1, 11.2).unwrap();

        assert_eq!(episodes[0]["window"], json!([9.1, 11.2]));
    }

    #[test]
    fn rejects_a_non_numeric_timestamp() {
        assert!(plan(vec![json!({"t": "12"})], 0.0, 60.0).is_err());
    }
}

#[cfg(test)]
mod parity {
    use serde_json::Value;

    fn fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
    }

    /// Границы эпизодов должны совпадать с сохранёнными эталонными окнами.
    #[test]
    fn matches_golden_episode_windows() {
        let dir = fixtures();
        let events: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.join("episode_events.json")).unwrap(),
        )
        .unwrap();
        let golden: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.join("episode_windows.json")).unwrap(),
        )
        .unwrap();
        let planned = super::plan(events, 0.0, 600.0).unwrap();
        assert_eq!(planned.len(), golden.len(), "число эпизодов");
        for (got, want) in planned.iter().zip(golden.iter()) {
            let got_window = got["window"].as_array().unwrap();
            let want_window = want["window"].as_array().unwrap();
            for i in 0..2 {
                let g = got_window[i].as_f64().unwrap();
                let w = want_window[i].as_f64().unwrap();
                assert!((g - w).abs() < 1e-3, "окно {i}: {g} != {w}");
            }
            let got_deaths: Vec<f64> = got["deaths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["t"].as_f64().unwrap())
                .collect();
            let want_deaths: Vec<f64> = want["deaths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_f64().unwrap())
                .collect();
            assert_eq!(got_deaths, want_deaths, "смерти в эпизоде");
        }
    }
}
