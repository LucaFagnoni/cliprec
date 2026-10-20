//! Riconoscimento degli IDR nei pacchetti Annex B, per marcare come keyframe solo i punti di
//! partenza validi di una clip (un I-frame non IDR o un CRA non lo sono).

use cliprec_core::VideoCodec;

/// Primo byte dopo il prossimo start code `00 00 01` a partire da `from`.
fn next_nal(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?.windows(3).position(|w| w == [0, 0, 1]).map(|p| from + p + 3)
}

/// Se il pacchetto è Annex B: `Some(true)` se il primo NAL VCL è IDR (H.264 tipo 5, HEVC tipi
/// 19/20), `Some(false)` altrimenti. `None` se non è Annex B o il codec non è H.264/HEVC.
pub(crate) fn first_vcl_is_idr(codec: VideoCodec, data: &[u8]) -> Option<bool> {
    if !(data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])) {
        return None;
    }
    let mut at = 0;
    while let Some(pos) = next_nal(data, at) {
        let header = *data.get(pos)?;
        match codec {
            VideoCodec::H264 => {
                let t = header & 0x1f;
                if (1..=5).contains(&t) {
                    return Some(t == 5);
                }
            }
            VideoCodec::Hevc => {
                let t = (header >> 1) & 0x3f;
                if t <= 31 {
                    return Some(t == 19 || t == 20);
                }
            }
            VideoCodec::Av1 => return None,
        }
        at = pos;
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
    }

    #[test]
    fn h264() {
        // AUD, SPS, PPS, SEI, poi la slice.
        let idr = au(&[&[0x09, 0xf0], &[0x67, 0x64, 0x00], &[0x68, 0xee], &[0x06, 0x05], &[0x65, 0x88]]);
        assert_eq!(first_vcl_is_idr(VideoCodec::H264, &idr), Some(true));
        let i_non_idr = au(&[&[0x09, 0xf0], &[0x41, 0x9a]]);
        assert_eq!(first_vcl_is_idr(VideoCodec::H264, &i_non_idr), Some(false));
        // Start code a 3 byte.
        assert_eq!(first_vcl_is_idr(VideoCodec::H264, &[0, 0, 1, 0x65, 0x88]), Some(true));
    }

    #[test]
    fn hevc() {
        // AUD (35), VPS (32), SPS (33), PPS (34), IDR_W_RADL (19).
        let idr = au(&[&[0x46, 0x01, 0x10], &[0x40, 0x01], &[0x42, 0x01], &[0x44, 0x01], &[0x26, 0x01, 0xaf]]);
        assert_eq!(first_vcl_is_idr(VideoCodec::Hevc, &idr), Some(true));
        assert_eq!(first_vcl_is_idr(VideoCodec::Hevc, &au(&[&[0x28, 0x01]])), Some(true)); // IDR_N_LP (20)
        assert_eq!(first_vcl_is_idr(VideoCodec::Hevc, &au(&[&[0x2a, 0x01]])), Some(false)); // CRA (21)
        assert_eq!(first_vcl_is_idr(VideoCodec::Hevc, &au(&[&[0x02, 0x01]])), Some(false)); // TRAIL_R (1)
    }

    #[test]
    fn not_annex_b() {
        assert_eq!(first_vcl_is_idr(VideoCodec::H264, &[0, 0, 0, 2, 0x65, 0x88]), None);
        assert_eq!(first_vcl_is_idr(VideoCodec::Hevc, &[]), None);
        assert_eq!(first_vcl_is_idr(VideoCodec::Av1, &[0, 0, 1, 0x12]), None);
    }
}
