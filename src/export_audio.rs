//! Decode the first WebM audio track and trim it on the replay video timeline.
use std::{fs::File, io::BufReader, path::Path};

use lewton::{
    audio::{PreviousWindowRight, read_audio_packet_generic},
    header::{IdentHeader, SetupHeader, read_header_ident, read_header_setup},
    samples::InterleavedSamples,
};
use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use opus_decoder::OpusDecoder;

#[derive(Debug)]
pub struct Audio {
    pub sample_rate: u32,
    pub channels: u32,
    pub pcm: Vec<i16>,
}

struct Vorbis {
    ident: IdentHeader,
    setup: SetupHeader,
    previous: PreviousWindowRight,
}

enum Decoder {
    Vorbis(Box<Vorbis>),
    Opus {
        decoder: Box<OpusDecoder>,
        channels: u32,
        gain: f32,
        delay_frames: u16,
    },
}

impl Decoder {
    fn open(codec: &str, private: &[u8]) -> Result<Self, String> {
        match codec {
            "A_VORBIS" => {
                let (ident, setup) = vorbis_headers(private)?;
                let ident =
                    read_header_ident(ident).map_err(|e| format!("заголовок Vorbis: {e}"))?;
                let setup = read_header_setup(
                    setup,
                    ident.audio_channels,
                    (ident.blocksize_0, ident.blocksize_1),
                )
                .map_err(|e| format!("конфигурация Vorbis: {e}"))?;
                Ok(Self::Vorbis(Box::new(Vorbis {
                    ident,
                    setup,
                    previous: PreviousWindowRight::new(),
                })))
            }
            "A_OPUS" => {
                if private.len() < 19 || !private.starts_with(b"OpusHead") {
                    return Err("отсутствует корректный OpusHead".into());
                }
                let channels = u32::from(private[9]);
                if !matches!(channels, 1 | 2) || private[18] != 0 {
                    return Err("поддерживается Opus mono/stereo с channel mapping 0".into());
                }
                let decoder = OpusDecoder::new(48_000, channels as usize)
                    .map_err(|e| format!("инициализация Opus: {e}"))?;
                let gain_db = f32::from(i16::from_le_bytes([private[16], private[17]])) / 256.0;
                Ok(Self::Opus {
                    decoder: Box::new(decoder),
                    channels,
                    gain: 10.0_f32.powf(gain_db / 20.0),
                    delay_frames: u16::from_le_bytes([private[10], private[11]]),
                })
            }
            _ => Err(format!("неподдерживаемая звуковая дорожка WebM: {codec}")),
        }
    }

    fn format(&self) -> (u32, u32) {
        match self {
            Self::Vorbis(state) => (
                state.ident.audio_sample_rate,
                u32::from(state.ident.audio_channels),
            ),
            Self::Opus { channels, .. } => (48_000, *channels),
        }
    }

    fn delay_ns(&self, container_delay: Option<u64>) -> Result<i64, String> {
        let delay = container_delay.unwrap_or_else(|| match self {
            Self::Opus { delay_frames, .. } => {
                (u64::from(*delay_frames) * 1_000_000_000 + 24_000) / 48_000
            }
            Self::Vorbis(_) => 0,
        });
        i64::try_from(delay).map_err(|_| "неверный CodecDelay аудио".into())
    }

    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<(), String> {
        match self {
            Self::Vorbis(state) => {
                let samples: InterleavedSamples<i16> = read_audio_packet_generic(
                    &state.ident,
                    &state.setup,
                    packet,
                    &mut state.previous,
                )
                .map_err(|e| format!("декодирование Vorbis: {e}"))?;
                *out = samples.samples;
            }
            Self::Opus {
                decoder,
                channels,
                gain,
                ..
            } => {
                out.resize(5760 * *channels as usize, 0);
                let frames = decoder
                    .decode(packet, out, false)
                    .map_err(|e| format!("декодирование Opus: {e}"))?;
                out.truncate(frames * *channels as usize);
                if *gain != 1.0 {
                    for sample in out {
                        *sample = (f32::from(*sample) * *gain).round() as i16;
                    }
                }
            }
        }
        Ok(())
    }
}

fn vorbis_headers(private: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let error = "повреждён Vorbis CodecPrivate с Xiph lacing";
    let Some((&2, mut remaining)) = private.split_first() else {
        return Err(error.into());
    };
    let mut headers_size = [0_usize; 2];
    for size in &mut headers_size {
        loop {
            let (&part, rest) = remaining.split_first().ok_or(error)?;
            remaining = rest;
            *size = size.checked_add(usize::from(part)).ok_or(error)?;
            if part != 255 {
                break;
            }
        }
    }
    let (ident, rest) = remaining.split_at_checked(headers_size[0]).ok_or(error)?;
    let (_, setup) = rest.split_at_checked(headers_size[1]).ok_or(error)?;
    Ok((ident, setup))
}

fn sample_index(ns: i64, rate: u32) -> i64 {
    let scaled = i128::from(ns) * i128::from(rate);
    ((scaled + scaled.signum() * 500_000_000) / 1_000_000_000) as i64
}

pub fn load(
    path: &Path,
    video_origin_ns: i64,
    start_ns: i64,
    end_ns: i64,
) -> Result<Option<Audio>, String> {
    if start_ns < 0 || end_ns <= start_ns {
        return Err("неверные границы аудио".into());
    }
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut demux = MatroskaFile::open(BufReader::new(file)).map_err(|e| e.to_string())?;
    let Some(track) = demux
        .tracks()
        .iter()
        .find(|track| track.track_type() == TrackType::Audio)
    else {
        return Ok(None);
    };
    let number = track.track_number().get();
    let codec = track.codec_id();
    let private = track
        .codec_private()
        .ok_or_else(|| format!("отсутствует CodecPrivate для {codec}"))?;
    let mut decoder = Decoder::open(codec, private)?;
    let (sample_rate, channels) = decoder.format();
    if !matches!(sample_rate, 44_100 | 48_000) || !matches!(channels, 1 | 2) {
        return Err(format!(
            "неподдерживаемый звук: {sample_rate} Гц, {channels} каналов"
        ));
    }
    let codec_delay = decoder.delay_ns(track.codec_delay())?;
    let scale = demux.info().timestamp_scale().get();
    let start = sample_index(
        video_origin_ns
            .checked_add(start_ns)
            .ok_or("переполнение времени аудио")?,
        sample_rate,
    );
    let length = sample_index(end_ns - start_ns, sample_rate);
    let end = start + length;
    let n = channels as usize;
    let value_count = usize::try_from(length)
        .ok()
        .and_then(|frames| frames.checked_mul(n))
        .ok_or("слишком большое аудиоокно")?;
    let mut pcm = Vec::new();
    pcm.try_reserve_exact(value_count)
        .map_err(|e| e.to_string())?;
    pcm.resize(value_count, 0);

    let mut frame = Frame::default();
    let mut decoded = Vec::new();
    let mut previous_tick = None;
    let mut next_sample: Option<i64> = None;
    while demux.next_frame(&mut frame).map_err(|e| e.to_string())? {
        if frame.track != number {
            continue;
        }
        let timestamp = frame
            .timestamp
            .checked_mul(scale)
            .and_then(|ns| i64::try_from(ns).ok())
            .ok_or("переполнение времени аудио")?;
        let nominal = sample_index(timestamp - codec_delay, sample_rate);
        // Matroska stores only the first timestamp of laced packets. Keep the
        // timeline continuous through a lace and through millisecond rounding.
        let at = next_sample
            .filter(|next| {
                previous_tick == Some(frame.timestamp)
                    || nominal.abs_diff(*next) <= u64::from(sample_rate / 500)
            })
            .unwrap_or(nominal);
        if at >= end {
            break;
        }
        decoder.decode(&frame.data, &mut decoded)?;
        let packet_end = at + (decoded.len() / n) as i64;
        previous_tick = Some(frame.timestamp);
        next_sample = Some(packet_end);
        let left = start.max(at);
        let right = end.min(packet_end);
        if left < right {
            let source = (left - at) as usize * n;
            let target = (left - start) as usize * n;
            let values = (right - left) as usize * n;
            pcm[target..target + values].copy_from_slice(&decoded[source..source + values]);
        }
    }
    Ok(Some(Audio {
        sample_rate,
        channels,
        pcm,
    }))
}
