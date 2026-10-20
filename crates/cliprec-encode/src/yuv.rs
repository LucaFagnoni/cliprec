//! NV12 (Y + UV interlacciato) → I420/yuv420p (Y, U, V separati), per l'encoder software.

/// Un piano di destinazione: byte e passo in byte.
pub(crate) struct Plane<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

/// Copia `w`×`h` di NV12 (`y`/`uv` con i loro passi in byte) nei piani yuv420p `dy`, `du`, `dv`.
/// Righe o piani più corti del necessario vengono troncati invece di andare in panico.
pub(crate) fn nv12_to_i420(w: usize, h: usize, (y, y_stride): (&[u8], usize), (uv, uv_stride): (&[u8], usize), dy: Plane, du: Plane, dv: Plane) {
    for (src, dst) in y.chunks(y_stride.max(1)).zip(dy.data.chunks_mut(dy.stride.max(1))).take(h) {
        let n = w.min(src.len()).min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
    }
    let rows = uv.chunks(uv_stride.max(1)).zip(du.data.chunks_mut(du.stride.max(1))).zip(dv.data.chunks_mut(dv.stride.max(1)));
    for ((src, u), v) in rows.take(h / 2) {
        for ((pair, u), v) in src.chunks_exact(2).zip(u.iter_mut()).zip(v.iter_mut()).take(w / 2) {
            *u = pair[0];
            *v = pair[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deinterleaves_uv_and_respects_strides() {
        // 4×2 con passo 6 in sorgente (2 byte di padding per riga).
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        let uv = [10, 20, 11, 21, 0, 0];
        let (mut dy, mut du, mut dv) = ([0u8; 10], [0u8; 3], [0u8; 3]);
        nv12_to_i420(
            4,
            2,
            (&y, 6),
            (&uv, 6),
            Plane { data: &mut dy, stride: 5 },
            Plane { data: &mut du, stride: 3 },
            Plane { data: &mut dv, stride: 3 },
        );
        assert_eq!(dy, [1, 2, 3, 4, 0, 5, 6, 7, 8, 0]);
        assert_eq!(du, [10, 11, 0]);
        assert_eq!(dv, [20, 21, 0]);
    }
}
