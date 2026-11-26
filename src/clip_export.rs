use std::{
    ffi::OsString,
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
};

use crate::video::{decoder::Picture, frames::render_rgb24, webm::Video};
use matroska_demuxer::{MatroskaFile, TrackType};
use serde_json::{Value, json};
use tempfile::TempPath;

use crate::{
    episodes,
    export_audio::{self, Audio},
    hud,
    mp4_sink::Mp4Sink,
};

const NS: i64 = 1_000_000_000;
const USAGE: &str = "cs2-replay-clips --video FILE.webm --save-dir DIR [--events FILE.json] [--start SECONDS] [--end SECONDS]";

pub fn run(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut path = None;
    let mut output = None;
    let mut events_path = None;
    let mut start = 0.0;
    let mut end = None;
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            println!("{USAGE}");
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("Для {} нужно значение", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--video") => path = Some(PathBuf::from(value)),
            Some("--save-dir") => output = Some(PathBuf::from(value)),
            Some("--events") => events_path = Some(PathBuf::from(value)),
            Some("--start") => start = seconds(value)?,
            Some("--end") => end = Some(seconds(value)?),
            _ => return Err(format!("Неизвестный параметр {}", flag.to_string_lossy())),
        }
    }
    let path = path.ok_or(USAGE)?;
    let output = output.ok_or(USAGE)?;
    let (duration, fps) = video_metadata(&path)?;
    let end = end.unwrap_or(duration);
    if !(0.0 <= start && start < end && end <= duration) {
        return Err(format!(
            "Нужен диапазон 0 <= start < end <= {duration:.6} с"
        ));
    }
    let events: Vec<Value> = if let Some(file) = events_path {
        serde_json::from_reader(BufReader::new(
            File::open(&file).map_err(|e| format!("{}: {e}", file.display()))?,
        ))
        .map_err(|e| format!("{}: {e}", file.display()))?
    } else {
        println!("Ищу смерти по HUD…");
        hud::detect(&path, start, end)?
    };
    let mut selected = Vec::new();
    for event in events {
        let time = event["t"]
            .as_f64()
            .ok_or("У события смерти отсутствует числовое поле t")?;
        if (start..=end).contains(&time) {
            selected.push(event);
        }
    }
    let count = selected.len();
    let mut episodes = episodes::plan(selected, start, end)?;

    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let staging = tempfile::Builder::new()
        .prefix(".vacnet-export-")
        .tempdir_in(&output)
        .map_err(|e| e.to_string())?;
    let mut number = 1;
    for episode in &mut episodes {
        let window = &episode["window"];
        let left = to_ns(window[0].as_f64().ok_or("отсутствует начало эпизода")?)?;
        let right = to_ns(window[1].as_f64().ok_or("отсутствует конец эпизода")?)?;
        let temp =
            TempPath::try_from_path(staging.path().join("clip.mp4")).map_err(|e| e.to_string())?;
        export_window(&path, &temp, left, right, fps)?;
        let exported = publish(
            temp,
            &output,
            |number| format!("episode_{number:03}.mp4"),
            &mut number,
        )?;
        episode["file"] = json!(
            exported
                .file_name()
                .expect("generated file name")
                .to_string_lossy()
        );
        println!("{}", exported.display());
    }
    let mut manifest = tempfile::Builder::new()
        .prefix(".vacnet-")
        .tempfile_in(&output)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(
        &mut manifest,
        &json!({
            "video": path, "model": null, "episodes": episodes,
        }),
    )
    .map_err(|e| e.to_string())?;
    manifest.flush().map_err(|e| e.to_string())?;
    let mut manifest_number = 1;
    let manifest = publish(
        manifest.into_temp_path(),
        &output,
        |number| {
            if number == 1 {
                "episodes.json".into()
            } else {
                format!("episodes_{number:03}.json")
            }
        },
        &mut manifest_number,
    )?;
    println!(
        "Смертей {count} · клипов {} · {}",
        episodes.len(),
        manifest.display()
    );
    Ok(())
}

fn seconds(value: OsString) -> Result<f64, String> {
    value
        .to_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|x| x.is_finite())
        .ok_or_else(|| format!("Некорректное время {value:?}"))
}

fn to_ns(seconds: f64) -> Result<i64, String> {
    let ticks = (seconds * NS as f64).round();
    if (0.0..(i64::MAX / 2) as f64).contains(&ticks) {
        Ok(ticks as i64)
    } else {
        Err("Некорректное время эпизода".into())
    }
}

fn video_metadata(path: &Path) -> Result<(f64, u32), String> {
    let reader = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let demux = MatroskaFile::open(BufReader::new(reader)).map_err(|e| e.to_string())?;
    let info = demux.info();
    let duration = info.duration().ok_or("Не указана длительность WebM")?
        * (info.timestamp_scale().get() as f64 / NS as f64);
    to_ns(duration)?;
    let track = demux
        .tracks()
        .iter()
        .find(|track| track.track_type() == TrackType::Video)
        .ok_or("В WebM отсутствует видеодорожка")?;
    let fps = track.default_duration().map_or(60, |duration| {
        (NS as f64 / duration.get() as f64)
            .round()
            .clamp(1.0, 240.0) as u32
    });
    Ok((duration, fps))
}

fn publish(
    mut temp: TempPath,
    output: &Path,
    name: impl Fn(usize) -> String,
    number: &mut usize,
) -> Result<PathBuf, String> {
    loop {
        let path = output.join(name(*number));
        *number += 1;
        match temp.persist_noclobber(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => temp = e.path,
            Err(e) => return Err(format!("{}: {}", path.display(), e.error)),
        }
    }
}

fn export_window(
    path: &Path,
    destination: &Path,
    start: i64,
    end: i64,
    fps: u32,
) -> Result<(), String> {
    let mut video = Video::open(path, 8).map_err(|e| e.to_string())?;
    let mut first = video
        .next_picture()
        .map_err(|e| e.to_string())?
        .ok_or("Нет кадров видео")?;
    let origin = first.pts_ns();
    let absolute_start = origin
        .checked_add(start)
        .ok_or("Переполнение времени эпизода")?;
    let stop = end - start;
    if stop <= 0 {
        return Err("Пустой эпизод".into());
    }
    if let Ok(Some((positioned, picture))) = Video::open_at_keyframe(path, 8, absolute_start) {
        video = positioned;
        first = picture;
    }
    let (width, height) = first.dimensions();
    let audio = export_audio::load(path, origin, start, end)?;
    let mut sink = Mp4Sink::new(
        destination,
        width as u32,
        height as u32,
        fps,
        audio.as_ref().map(|a| (a.sample_rate, a.channels)),
    )?;
    let mut audio = audio
        .as_ref()
        .map(|source| AudioCursor { source, index: 0 });
    let mut pending = first;
    let mut previous = pending.pts_ns() - absolute_start;
    let mut pts = 0;
    let mut rgb = Vec::new();
    while let Some(picture) = video.next_picture().map_err(|e| e.to_string())? {
        let next = picture.pts_ns() - absolute_start;
        if next < previous {
            return Err("Время кадров AV1 идёт в обратном порядке".into());
        }
        previous = next;
        if next >= stop {
            break;
        }
        if next > pts {
            write_video(
                &mut sink,
                &mut audio,
                &pending,
                &mut rgb,
                (width, height),
                pts,
                next,
            )?;
            pts = next;
        }
        pending = picture;
    }
    write_video(
        &mut sink,
        &mut audio,
        &pending,
        &mut rgb,
        (width, height),
        pts,
        stop,
    )?;
    if let Some(audio) = &mut audio {
        audio.write_until(&mut sink, i64::MAX)?;
    }
    sink.finish()
}

fn write_video(
    sink: &mut Mp4Sink,
    audio: &mut Option<AudioCursor<'_>>,
    picture: &Picture,
    rgb: &mut Vec<u8>,
    dimensions: (usize, usize),
    pts: i64,
    next: i64,
) -> Result<(), String> {
    if picture.dimensions() != dimensions {
        return Err("Размер кадра изменился внутри эпизода".into());
    }
    if let Some(audio) = audio {
        audio.write_until(sink, pts)?;
    }
    render_rgb24(picture, rgb).map_err(|e| e.to_string())?;
    sink.video(rgb, pts, next - pts)
}

struct AudioCursor<'a> {
    source: &'a Audio,
    index: usize,
}

impl AudioCursor<'_> {
    fn write_until(&mut self, sink: &mut Mp4Sink, until: i64) -> Result<(), String> {
        let channels = self.source.channels as usize;
        while self.index < self.source.pcm.len() {
            let pts = (self.index / channels) as i64 * NS / i64::from(self.source.sample_rate);
            if pts > until {
                break;
            }
            let end = (self.index + channels * 1024).min(self.source.pcm.len());
            sink.audio(&self.source.pcm[self.index..end], pts)?;
            self.index = end;
        }
        Ok(())
    }
}
