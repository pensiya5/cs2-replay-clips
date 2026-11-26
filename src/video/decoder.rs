use std::{mem::MaybeUninit, ptr::NonNull, slice};

use rav1d::include::dav1d::{
    data::Dav1dData,
    dav1d::{Dav1dContext, Dav1dSettings},
    headers::{
        DAV1D_FRAME_TYPE_KEY, DAV1D_PIXEL_LAYOUT_I420, Dav1dMatrixCoefficients, Dav1dSequenceHeader,
    },
    picture::Dav1dPicture,
};
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_parse_sequence_header, dav1d_picture_unref, dav1d_send_data,
};

use super::Result;

pub(crate) fn has_sequence_header(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let mut header = MaybeUninit::<Dav1dSequenceHeader>::uninit();
    // SAFETY: rav1d reads the packet slice and writes only to the header output.
    let status = unsafe {
        dav1d_parse_sequence_header(
            Some(NonNull::from(&mut header).cast()),
            NonNull::new(bytes.as_ptr().cast_mut()),
            bytes.len(),
        )
    };
    status.0 == 0
}

pub struct Packet(Dav1dData);

impl Packet {
    pub fn copy_from(bytes: &[u8], pts_ns: i64) -> Result<Self> {
        if bytes.is_empty() {
            return Err("empty AV1 packet".into());
        }
        let mut packet = Self(Dav1dData::default());
        // SAFETY: The out pointer is writable. On success rav1d allocates bytes.len() bytes.
        let dst = unsafe { dav1d_data_create(Some(NonNull::from(&mut packet.0)), bytes.len()) };
        if dst.is_null() {
            return Err("dav1d_data_create failed".into());
        }
        // SAFETY: The source slice and the new allocation do not overlap.
        unsafe { dst.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
        packet.0.m.timestamp = pts_ns;
        Ok(packet)
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: The packet holds either a live reference or an empty value after send.
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut self.0))) };
    }
}

pub struct Decoder(Option<Dav1dContext>);

impl Decoder {
    pub fn open(threads: i32) -> Result<Self> {
        if !(0..=256).contains(&threads) {
            return Err("thread count must be in 0..=256 (0 = automatic)".into());
        }
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: The initializer writes the entire value before it is read.
        let mut settings = unsafe {
            dav1d_default_settings(NonNull::from(&mut settings).cast());
            settings.assume_init()
        };
        settings.n_threads = threads;
        settings.all_layers = 0;
        settings.frame_size_limit = 4096 * 2304;
        let mut decoder = Self(None);
        // SAFETY: Both arguments point to initialized, writable local values.
        let status = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut decoder.0)),
                Some(NonNull::from(&mut settings)),
            )
        };
        check(status.0, "dav1d_open")?;
        Ok(decoder)
    }

    /// A false result leaves the packet owned by the caller for retry.
    pub fn send(&mut self, packet: &mut Packet) -> Result<bool> {
        // SAFETY: The decoder is open and the packet holds a live reference.
        let status = unsafe { dav1d_send_data(self.0, Some(NonNull::from(&mut packet.0))) };
        if status.0 == -libc::EAGAIN {
            return Ok(false);
        }
        check(status.0, "dav1d_send_data")?;
        Ok(true)
    }

    pub fn receive(&mut self) -> Result<Option<Picture>> {
        let mut picture = Picture {
            raw: Dav1dPicture::default(),
            matrix_hint: None,
        };
        // SAFETY: The context is open and the picture is an empty, writable out parameter.
        let status = unsafe { dav1d_get_picture(self.0, Some(NonNull::from(&mut picture.raw))) };
        if status.0 == -libc::EAGAIN {
            return Ok(None);
        }
        check(status.0, "dav1d_get_picture")?;
        Ok(Some(picture))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: This is the only owner of the handle from dav1d_open.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.0))) };
    }
}

fn check(status: i32, call: &str) -> Result<()> {
    if status != 0 {
        return Err(format!("{call} returned {status}").into());
    }
    Ok(())
}

pub struct Picture {
    raw: Dav1dPicture,
    pub(crate) matrix_hint: Option<Dav1dMatrixCoefficients>,
}

pub struct Plane<'picture> {
    pub bytes: &'picture [u8],
    pub stride: usize,
}

impl Picture {
    pub(crate) fn is_keyframe(&self) -> bool {
        self.raw.frame_hdr.is_some_and(|header| {
            // SAFETY: The picture owns a reference to its frame header.
            let header = unsafe { header.as_ref() };
            header.frame_type == DAV1D_FRAME_TYPE_KEY && header.show_existing_frame == 0
        })
    }

    pub fn pts_ns(&self) -> i64 {
        self.raw.m.timestamp
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.raw.p.w as usize, self.raw.p.h as usize)
    }

    pub fn sequence_header(&self) -> Result<&Dav1dSequenceHeader> {
        let header = self.raw.seq_hdr.ok_or("missing AV1 sequence header")?;
        // SAFETY: The picture owns a reference to its sequence header.
        Ok(unsafe { header.as_ref() })
    }

    pub fn i420_planes(&self) -> Result<[Plane<'_>; 3]> {
        if self.raw.p.layout != DAV1D_PIXEL_LAYOUT_I420 || self.raw.p.bpc != 8 {
            return Err(format!(
                "RGB export requires 8-bit I420, got layout={}, depth={}",
                self.raw.p.layout, self.raw.p.bpc
            )
            .into());
        }
        let (w, h) = self.dimensions();
        Ok([
            self.plane(0, w, h)?,
            self.plane(1, w.div_ceil(2), h.div_ceil(2))?,
            self.plane(2, w.div_ceil(2), h.div_ceil(2))?,
        ])
    }

    fn plane(&self, index: usize, width: usize, height: usize) -> Result<Plane<'_>> {
        let stride = usize::try_from(self.raw.stride[usize::from(index != 0)])?;
        let data = self.raw.data[index]
            .ok_or("missing picture plane")?
            .cast::<u8>();
        if width == 0 || height == 0 || stride < width {
            return Err("invalid AV1 plane dimensions".into());
        }
        let len = stride
            .checked_mul(height)
            .ok_or("AV1 plane size overflow")?;
        if len > isize::MAX as usize {
            return Err("AV1 plane exceeds maximum slice size".into());
        }
        // SAFETY: The default rav1d allocator owns initialized, stride-padded planes.
        // The picture's reference keeps the plane alive for this borrow.
        let bytes = unsafe { slice::from_raw_parts(data.as_ptr(), len) };
        Ok(Plane { bytes, stride })
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: This picture owns the reference returned by dav1d_get_picture, if any.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.raw))) };
    }
}
