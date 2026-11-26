use rav1d::include::dav1d::headers::DAV1D_MC_UNKNOWN;

use super::{Result, decoder::Picture};

pub fn render_rgb24(picture: &Picture, rgb: &mut Vec<u8>) -> Result<()> {
    let (width, height) = picture.dimensions();
    let [y, u, v] = picture.i420_planes()?;
    let header = picture.sequence_header()?;
    let matrix = if header.mtrx == DAV1D_MC_UNKNOWN {
        picture.matrix_hint.unwrap_or(DAV1D_MC_UNKNOWN)
    } else {
        header.mtrx
    };
    let (kr, kb) = match matrix {
        1 => (0.2126, 0.0722),
        2 | 5 | 6 => (0.299, 0.114),
        9 => (0.2627, 0.0593),
        value => return Err(format!("unsupported AV1 matrix coefficients {value}").into()),
    };
    let (y_offset, y_gain, chroma_gain) = if header.color_range == 0 {
        (16, 255.0 / 219.0, 255.0 / 224.0)
    } else {
        (0, 1.0, 1.0)
    };
    let kg = 1.0 - kr - kb;
    // The PyAV x86 reference uses a 13-bit matrix, floors each term separately
    // and shares each chroma sample across a 2×2 block when exporting at native size.
    const SCALE: i32 = 1 << 13;
    let fixed = |coefficient: f64| (coefficient * f64::from(SCALE)).round() as i32;
    let y_coeff = fixed(y_gain);
    let red_v = fixed(2.0 * (1.0 - kr) * chroma_gain);
    let blue_u = fixed(2.0 * (1.0 - kb) * chroma_gain);
    let green_u = fixed(-2.0 * (1.0 - kb) * kb / kg * chroma_gain);
    let green_v = fixed(-2.0 * (1.0 - kr) * kr / kg * chroma_gain);

    let rgb_stride = width * 3;
    rgb.resize(rgb_stride * height, 0);
    let clamp = |value: i32| value.clamp(0, 255) as u8;
    for (row, output) in rgb.chunks_exact_mut(rgb_stride).enumerate() {
        let luma = &y.bytes[row * y.stride..][..width];
        let u_row = &u.bytes[row / 2 * u.stride..][..width.div_ceil(2)];
        let v_row = &v.bytes[row / 2 * v.stride..][..width.div_ceil(2)];
        for ((&cb, &cr), (y_pair, pixels)) in u_row
            .iter()
            .zip(v_row)
            .zip(luma.chunks(2).zip(output.chunks_mut(6)))
        {
            let cb = i32::from(cb) - 128;
            let cr = i32::from(cr) - 128;
            let r = (red_v * cr) >> 13;
            let g = ((green_u * cb) >> 13) + ((green_v * cr) >> 13);
            let b = (blue_u * cb) >> 13;
            for (&sample, pixel) in y_pair.iter().zip(pixels.as_chunks_mut::<3>().0) {
                let luma = ((i32::from(sample) - y_offset) * y_coeff) >> 13;
                *pixel = [clamp(luma + r), clamp(luma + g), clamp(luma + b)];
            }
        }
    }
    Ok(())
}
