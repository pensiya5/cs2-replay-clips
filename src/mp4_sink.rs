//! H.264/AAC MP4 writer backed by the Windows Media Foundation encoders.

use std::{
    marker::PhantomData,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{self, null_mut},
    rc::Rc,
};

use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
    },
    core::{GUID, PCWSTR},
};

type Result<T> = std::result::Result<T, String>;

fn mf<T>(operation: &str, result: windows::core::Result<T>) -> Result<T> {
    result.map_err(|error| format!("{operation}: {error}"))
}

struct Runtime(PhantomData<Rc<()>>);

impl Runtime {
    fn start() -> Result<Self> {
        // SAFETY: Thread-local COM initialization is balanced in Drop.
        unsafe {
            mf(
                "CoInitializeEx",
                CoInitializeEx(None, COINIT_MULTITHREADED).ok(),
            )?;
            if let Err(error) = mf("MFStartup", MFStartup(MF_VERSION, MFSTARTUP_FULL)) {
                CoUninitialize();
                return Err(error);
            }
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: This guard is tied to the initializing thread. The writer
        // and its COM references are released before shutdown.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

pub struct Mp4Sink {
    writer: IMFSinkWriter,
    video_stream: u32,
    audio_stream: Option<AudioStream>,
    width: usize,
    height: usize,
    nv12: Vec<u8>,
    _runtime: Runtime,
}

struct AudioStream {
    index: u32,
    rate: u32,
    channels: usize,
}

impl Mp4Sink {
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        audio: Option<(u32, u32)>,
    ) -> Result<Self> {
        if !(2..=8192).contains(&width)
            || !(2..=8192).contains(&height)
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || fps == 0
        {
            return Err("H.264 requires positive even dimensions and a positive frame rate".into());
        }
        if audio.is_some_and(|(rate, channels)| {
            !matches!(rate, 44_100 | 48_000) || !matches!(channels, 1 | 2)
        }) {
            return Err("AAC output requires 44.1 or 48 kHz mono/stereo PCM".into());
        }
        let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err("MP4 path contains a NUL character".into());
        }
        wide.push(0);
        let runtime = Runtime::start()?;
        // SAFETY: The URL buffer is NUL-terminated and lives through this call.
        // COM and Media Foundation have been initialized for this thread.
        let writer = unsafe {
            mf(
                "MFCreateSinkWriterFromURL",
                MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, None),
            )?
        };
        let video_stream = configure_video(&writer, width, height, fps)?;
        let audio_stream = audio
            .map(|(rate, channels)| configure_audio(&writer, rate, channels))
            .transpose()?;
        // SAFETY: All stream types have been configured on this sink writer.
        unsafe { mf("BeginWriting", writer.BeginWriting())? };
        let (width, height) = (width as usize, height as usize);
        Ok(Self {
            writer,
            video_stream,
            audio_stream,
            width,
            height,
            nv12: vec![0; width * height * 3 / 2],
            _runtime: runtime,
        })
    }

    pub fn video(&mut self, rgb: &[u8], pts_ns: i64, duration_ns: i64) -> Result<()> {
        if rgb.len() != self.width * self.height * 3 {
            return Err("RGB frame size differs from the configured video size".into());
        }
        rgb_to_nv12(rgb, &mut self.nv12, self.width, self.height);
        write_sample(
            &self.writer,
            self.video_stream,
            &self.nv12,
            pts_ns,
            duration_ns,
        )
    }

    pub fn audio(&mut self, pcm: &[i16], pts_ns: i64) -> Result<()> {
        let stream = self
            .audio_stream
            .as_ref()
            .ok_or("MP4 has no configured audio stream")?;
        if !pcm.len().is_multiple_of(stream.channels) || pcm.is_empty() {
            return Err("PCM buffer must contain complete audio frames".into());
        }
        let frames = i128::try_from(pcm.len() / stream.channels).map_err(|err| err.to_string())?;
        let duration_ns = i64::try_from(
            (frames * 1_000_000_000 + i128::from(stream.rate / 2)) / i128::from(stream.rate),
        )
        .map_err(|err| err.to_string())?;
        // SAFETY: Windows uses little-endian i16. All elements are initialized,
        // and the byte view has the same bounds and lifetime as the PCM slice.
        let bytes = unsafe {
            std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), std::mem::size_of_val(pcm))
        };
        write_sample(&self.writer, stream.index, bytes, pts_ns, duration_ns)
    }

    pub fn finish(self) -> Result<()> {
        // SAFETY: The writer is initialized and owned by this thread.
        unsafe { mf("Finalize MP4", self.writer.Finalize()) }
    }
}

fn configure_video(writer: &IMFSinkWriter, width: u32, height: u32, fps: u32) -> Result<u32> {
    // SAFETY: Media Foundation is active and the media types remain alive for
    // their COM calls. Packed attributes store their numerator in the high bits.
    unsafe {
        let output = mf("create H.264 media type", MFCreateMediaType())?;
        video_type(&output, &MFVideoFormat_H264, width, height, fps)?;
        let bitrate = ((u64::from(width) * u64::from(height)).saturating_mul(u64::from(fps)) / 8)
            .clamp(500_000, 24_000_000) as u32;
        mf(
            "set H.264 bitrate",
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate),
        )?;
        mf(
            "set H.264 profile",
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32),
        )?;
        let stream = mf("AddStream(H.264)", writer.AddStream(&output))?;
        let input = mf("create NV12 media type", MFCreateMediaType())?;
        video_type(&input, &MFVideoFormat_NV12, width, height, fps)?;
        mf(
            "set NV12 stride",
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width),
        )?;
        mf(
            "SetInputMediaType(NV12)",
            writer.SetInputMediaType(stream, &input, None),
        )?;
        Ok(stream)
    }
}

fn video_type(
    media_type: &IMFMediaType,
    format: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    // SAFETY: Attribute keys and values are valid for this Media Foundation type.
    unsafe {
        mf(
            "set video major type",
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video),
        )?;
        mf(
            "set video subtype",
            media_type.SetGUID(&MF_MT_SUBTYPE, format),
        )?;
        mf(
            "set video dimensions",
            media_type.SetUINT64(&MF_MT_FRAME_SIZE, pair(width, height)),
        )?;
        mf(
            "set video frame rate",
            media_type.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1)),
        )?;
        mf(
            "set pixel aspect ratio",
            media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1)),
        )?;
        mf(
            "set progressive scan",
            media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32),
        )?;
        mf(
            "set BT.709 matrix",
            media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32),
        )?;
        mf(
            "set BT.709 primaries",
            media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32),
        )?;
        mf(
            "set BT.709 transfer",
            media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32),
        )?;
        mf(
            "set nominal video range",
            media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32),
        )
    }
}

fn configure_audio(writer: &IMFSinkWriter, rate: u32, channels: u32) -> Result<AudioStream> {
    // SAFETY: Both types supply the attributes required by the AAC encoder.
    unsafe {
        let output = mf("create AAC media type", MFCreateMediaType())?;
        audio_type(&output, &MFAudioFormat_AAC, rate, channels)?;
        mf(
            "set AAC bitrate",
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000),
        )?;
        mf(
            "set raw AAC payload",
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0),
        )?;
        let index = mf("AddStream(AAC)", writer.AddStream(&output))?;
        let input = mf("create PCM media type", MFCreateMediaType())?;
        audio_type(&input, &MFAudioFormat_PCM, rate, channels)?;
        mf(
            "set PCM block alignment",
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2),
        )?;
        mf(
            "set PCM byte rate",
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2),
        )?;
        mf(
            "SetInputMediaType(PCM)",
            writer.SetInputMediaType(index, &input, None),
        )?;
        Ok(AudioStream {
            index,
            rate,
            channels: channels as usize,
        })
    }
}

fn audio_type(media_type: &IMFMediaType, format: &GUID, rate: u32, channels: u32) -> Result<()> {
    // SAFETY: Attribute keys and values are valid for this Media Foundation type.
    unsafe {
        mf(
            "set audio major type",
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio),
        )?;
        mf(
            "set audio subtype",
            media_type.SetGUID(&MF_MT_SUBTYPE, format),
        )?;
        mf(
            "set audio sample rate",
            media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate),
        )?;
        mf(
            "set audio channel count",
            media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels),
        )?;
        mf(
            "set audio sample width",
            media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16),
        )
    }
}

fn pair(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn write_sample(
    writer: &IMFSinkWriter,
    stream: u32,
    bytes: &[u8],
    pts_ns: i64,
    duration_ns: i64,
) -> Result<()> {
    if pts_ns < 0 || duration_ns <= 0 {
        return Err("MP4 samples require a nonnegative PTS and positive duration".into());
    }
    let end_ns = pts_ns
        .checked_add(duration_ns)
        .ok_or("MP4 sample time overflow")?;
    let time = pts_ns / 100;
    let duration = end_ns / 100 - time;
    if duration == 0 {
        return Err("MP4 sample duration is shorter than 100 ns".into());
    }
    let size = u32::try_from(bytes.len()).map_err(|error| error.to_string())?;
    // SAFETY: A successfully locked memory buffer has at least the capacity
    // requested from MFCreateMemoryBuffer. The data is copied before unlocking.
    unsafe {
        let buffer = mf("MFCreateMemoryBuffer", MFCreateMemoryBuffer(size))?;
        let mut dest = null_mut();
        mf("lock sample buffer", buffer.Lock(&mut dest, None, None))?;
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
        mf("unlock sample buffer", buffer.Unlock())?;
        mf("set sample buffer size", buffer.SetCurrentLength(size))?;
        let sample = mf("MFCreateSample", MFCreateSample())?;
        mf("add sample buffer", sample.AddBuffer(&buffer))?;
        mf("set sample PTS", sample.SetSampleTime(time))?;
        mf("set sample duration", sample.SetSampleDuration(duration))?;
        mf("WriteSample", writer.WriteSample(stream, &sample))
    }
}

fn rgb_to_nv12(rgb: &[u8], nv12: &mut [u8], width: usize, height: usize) {
    let pixels = width * height;
    let (y, uv) = nv12.split_at_mut(pixels);
    // Integer BT.709 limited-range coefficients scaled by 256.
    for (source, luma) in rgb.chunks_exact(3).zip(y) {
        let (r, g, b) = (
            i32::from(source[0]),
            i32::from(source[1]),
            i32::from(source[2]),
        );
        *luma = (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)).clamp(16, 235) as u8;
    }
    for (pair, chroma) in rgb
        .chunks_exact(2 * width * 3)
        .zip(uv.chunks_exact_mut(width))
    {
        let (top, bottom) = pair.split_at(width * 3);
        for ((top, bottom), dest) in top
            .chunks_exact(6)
            .zip(bottom.chunks_exact(6))
            .zip(chroma.chunks_exact_mut(2))
        {
            let sum = |i| {
                i32::from(top[i])
                    + i32::from(top[i + 3])
                    + i32::from(bottom[i])
                    + i32::from(bottom[i + 3])
            };
            let (r, g, b) = (sum(0), sum(1), sum(2));
            dest[0] = (128 + ((-26 * r - 87 * g + 112 * b + 512) >> 10)).clamp(16, 240) as u8;
            dest[1] = (128 + ((112 * r - 102 * g - 10 * b + 512) >> 10)).clamp(16, 240) as u8;
        }
    }
}
