use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use matroska_demuxer::{Frame, MatrixCoefficients, MatroskaFile, TrackType};
use rav1d::include::dav1d::headers::Dav1dMatrixCoefficients;

use super::{
    Result,
    decoder::{Decoder, Packet, Picture, has_sequence_header},
};

pub struct Video {
    demux: MatroskaFile<BufferedFile>,
    decoder: Decoder,
    frame: Frame,
    track: u64,
    timestamp_scale_ns: u64,
    matrix_hint: Option<Dav1dMatrixCoefficients>,
    pending: Option<Packet>,
    preroll: VecDeque<(Vec<u8>, i64)>,
    frame_ready: bool,
    eof: bool,
}

impl Video {
    pub fn open(path: &Path, threads: i32) -> Result<Self> {
        let demux = MatroskaFile::open(BufferedFile::open(path)?)?;
        let track = demux
            .tracks()
            .iter()
            .find(|t| t.track_type() == TrackType::Video)
            .ok_or("video track missing")?;
        if track.codec_id() != "V_AV1" {
            return Err(format!("expected AV1, got {}", track.codec_id()).into());
        }
        if track.codec_delay().unwrap_or(0) != 0 {
            return Err("nonzero video CodecDelay is unsupported".into());
        }
        let config = config_obus(track.codec_private().unwrap_or_default())?;
        let pending = if config.is_empty() {
            None
        } else {
            Some(Packet::copy_from(config, i64::MIN)?)
        };
        Ok(Self {
            decoder: Decoder::open(threads)?,
            frame: Frame::default(),
            track: track.track_number().get(),
            timestamp_scale_ns: demux.info().timestamp_scale().get(),
            matrix_hint: track
                .video()
                .and_then(|video| video.colour())
                .and_then(|colour| colour.matrix_coefficients())
                .map(matrix_code),
            pending,
            preroll: VecDeque::new(),
            frame_ready: false,
            demux,
            eof: false,
        })
    }

    /// Start on a validated keyframe at or before pts_ns, if one is found.
    pub fn open_at_keyframe(
        path: &Path,
        threads: i32,
        pts_ns: i64,
    ) -> Result<Option<(Self, Picture)>> {
        let search_start = pts_ns.saturating_sub(10_000_000_000);
        if search_start <= 0 {
            return Ok(None);
        }
        if let Ok(Some(positioned)) = Self::seek_near_keyframe(path, threads, search_start, pts_ns)
        {
            return Ok(Some(positioned));
        }
        Self::scan_for_keyframe(path, threads, pts_ns)
    }

    fn seek_near_keyframe(
        path: &Path,
        threads: i32,
        search_start: i64,
        target: i64,
    ) -> Result<Option<(Self, Picture)>> {
        let mut video = Self::open(path, threads)?;
        video
            .demux
            .seek(u64::try_from(search_start)? / video.timestamp_scale_ns)?;
        while video.demux.next_frame(&mut video.frame)? {
            if video.frame.track != video.track {
                continue;
            }
            let packet_pts = video.packet_pts()?;
            if packet_pts > target {
                break;
            }
            if video.is_keyframe_candidate() {
                video.frame_ready = true;
                return video.start_on_keyframe(packet_pts);
            }
        }
        Ok(None)
    }

    // A Cue can point to an unusable offset in matroska-demuxer 0.8.1.
    // Scan compressed packets and retain only the current GOP through target.
    fn scan_for_keyframe(
        path: &Path,
        threads: i32,
        target: i64,
    ) -> Result<Option<(Self, Picture)>> {
        const MAX_PREROLL_BYTES: usize = 64 * 1024 * 1024;
        let mut video = Self::open(path, threads)?;
        let mut key_pts = None;
        let mut bytes = 0_usize;
        while video.demux.next_frame(&mut video.frame)? {
            if video.frame.track != video.track {
                continue;
            }
            let pts = video.packet_pts()?;
            if pts > target {
                video.frame_ready = true;
                break;
            }
            if video.is_keyframe_candidate() {
                key_pts = Some(pts);
                bytes = 0;
                video.preroll.clear();
            }
            if key_pts.is_some() {
                bytes = bytes.saturating_add(video.frame.data.len());
                if bytes > MAX_PREROLL_BYTES {
                    key_pts = None;
                    video.preroll.clear();
                } else {
                    video.preroll.push_back((video.frame.data.clone(), pts));
                }
            }
        }
        match key_pts {
            Some(pts) => video.start_on_keyframe(pts),
            None => Ok(None),
        }
    }

    fn is_keyframe_candidate(&self) -> bool {
        !self.frame.is_invisible
            && self
                .frame
                .is_keyframe
                .unwrap_or_else(|| has_sequence_header(&self.frame.data))
    }

    fn start_on_keyframe(mut self, key_pts: i64) -> Result<Option<(Self, Picture)>> {
        if let Some(first) = self.next_picture()?
            && first.pts_ns() == key_pts
            && first.is_keyframe()
        {
            return Ok(Some((self, first)));
        }
        Ok(None)
    }

    pub fn next_picture(&mut self) -> Result<Option<Picture>> {
        loop {
            if self.pending.is_none() && !self.eof {
                self.pending = self.next_packet()?;
                self.eof = self.pending.is_none();
            }
            if let Some(packet) = &mut self.pending
                && self.decoder.send(packet)?
            {
                self.pending = None;
            }
            // One receive per send attempt preserves frame-thread pipelining.
            // After demux EOF, keep receiving until EAGAIN ends the drain.
            let mut picture = self.decoder.receive()?;
            if let Some(picture) = &mut picture {
                picture.matrix_hint = self.matrix_hint;
            }
            if picture.is_some() || self.eof {
                return Ok(picture);
            }
        }
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if let Some((bytes, pts)) = self.preroll.pop_front() {
            return Packet::copy_from(&bytes, pts).map(Some);
        }
        if std::mem::take(&mut self.frame_ready) {
            return self.current_packet().map(Some);
        }
        while self.demux.next_frame(&mut self.frame)? {
            if self.frame.track == self.track {
                return self.current_packet().map(Some);
            }
        }
        Ok(None)
    }

    fn current_packet(&self) -> Result<Packet> {
        Packet::copy_from(&self.frame.data, self.packet_pts()?)
    }

    fn packet_pts(&self) -> Result<i64> {
        let pts = self
            .frame
            .timestamp
            .checked_mul(self.timestamp_scale_ns)
            .ok_or("Matroska timestamp overflow")?;
        Ok(i64::try_from(pts)?)
    }
}

fn matrix_code(matrix: MatrixCoefficients) -> Dav1dMatrixCoefficients {
    match matrix {
        MatrixCoefficients::Identity => 0,
        MatrixCoefficients::Bt709 => 1,
        MatrixCoefficients::Unknown => 2,
        MatrixCoefficients::Fcc73682 => 4,
        MatrixCoefficients::Bt470bg => 5,
        MatrixCoefficients::Smpte170 => 6,
        MatrixCoefficients::Smpte240 => 7,
        MatrixCoefficients::YCoCg => 8,
        MatrixCoefficients::Bt2020Ncl => 9,
        MatrixCoefficients::Bt2020Cl => 10,
        MatrixCoefficients::SmpteSt2085 => 11,
        MatrixCoefficients::ChromaDerivedNcl => 12,
        MatrixCoefficients::ChromaDerivedCl => 13,
        MatrixCoefficients::Bt2100 => 14,
    }
}

fn config_obus(codec_private: &[u8]) -> Result<&[u8]> {
    if codec_private.is_empty() || codec_private == [0x03, 0x00, 0x34, 0x00] {
        // Valve clips use this four-byte record and carry the sequence header in-band.
        return Ok(&[]);
    }
    if codec_private.len() < 4 || codec_private[0] != 0x81 {
        return Err("invalid AV1CodecConfigurationRecord (expected version 1)".into());
    }
    Ok(&codec_private[4..])
}

// The demuxer seeks to EOF and back to check each element's declared size.
// Delay these logical seeks so they do not invalidate the read buffer.
struct BufferedFile {
    reader: BufReader<File>,
    length: u64,
    position: u64,
    reader_position: u64,
}

impl BufferedFile {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            reader: BufReader::with_capacity(1024 * 1024, file),
            length,
            position: 0,
            reader_position: 0,
        })
    }
}

impl Seek for BufferedFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.length.checked_add_signed(delta),
        };
        self.position = position.ok_or(io::ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}

impl Read for BufferedFile {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.reader_position != self.position {
            let delta = i128::from(self.position) - i128::from(self.reader_position);
            match i64::try_from(delta) {
                Ok(delta) => self.reader.seek_relative(delta)?,
                Err(_) => {
                    self.reader.seek(SeekFrom::Start(self.position))?;
                }
            }
            self.reader_position = self.position;
        }
        let read = self.reader.read(dst)?;
        self.position += read as u64;
        self.reader_position = self.position;
        Ok(read)
    }
}
