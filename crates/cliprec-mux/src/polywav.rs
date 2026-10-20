//! WAV polifonico (m27): un solo file con 2 canali (L, R) per traccia, PCM 24 bit,
//! `WAVE_FORMAT_EXTENSIBLE` con `dwChannelMask = 0` (canali senza posizione) e i nomi dei canali
//! in un chunk iXML. REAPER lo divide in tracce, Audition legge i nomi. Scritto in streaming.

use std::io::{BufWriter, Write};
use std::path::Path;

use cliprec_core::{CHANNELS, SAMPLE_RATE};

use crate::{ClipAudio, MuxError, io};

const BLOCK: usize = 4096;
const S24_MAX: f32 = 8_388_607.0;
/// SubFormat KSDATAFORMAT_SUBTYPE_PCM.
const PCM_GUID: [u8; 16] = [0x01, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71];

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Chunk iXML con un `TRACK` per canale: «Nome L», «Nome R».
fn ixml<A: ClipAudio + ?Sized>(audio: &A) -> String {
    let mut x = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><BWFXML><IXML_VERSION>1.5</IXML_VERSION><TRACK_LIST>"#);
    x.push_str(&format!("<TRACK_COUNT>{}</TRACK_COUNT>", audio.track_count() * CHANNELS));
    for t in 0..audio.track_count() {
        let name = xml_escape(audio.track_name(t));
        for (c, side) in ["L", "R"].iter().enumerate() {
            let index = t * CHANNELS + c + 1;
            x.push_str(&format!(
                "<TRACK><CHANNEL_INDEX>{index}</CHANNEL_INDEX><INTERLEAVE_INDEX>{index}</INTERLEAVE_INDEX><NAME>{name} {side}</NAME></TRACK>"
            ));
        }
    }
    x.push_str("</TRACK_LIST></BWFXML>");
    x
}

pub(crate) fn write<A: ClipAudio + ?Sized>(path: &Path, audio: &A) -> Result<(), MuxError> {
    let tracks = audio.track_count();
    let channels = tracks * CHANNELS;
    let block_align = channels * 3;
    let frames = audio.frames();
    let ixml = ixml(audio).into_bytes();
    let pad = |n: usize| n % 2;
    let data_len = frames as u64 * block_align as u64;
    let riff_len = 4 + (8 + 40) + (8 + data_len + pad(data_len as usize) as u64) + (8 + (ixml.len() + pad(ixml.len())) as u64);
    let (Ok(riff_len), Ok(data_len32), Ok(channels16), Ok(block_align16)) = (
        u32::try_from(riff_len),
        u32::try_from(data_len),
        u16::try_from(channels),
        u16::try_from(block_align),
    ) else {
        return Err(MuxError::TooLarge("WAV polifonico oltre 4 GB o con troppi canali"));
    };

    let file = std::fs::File::create(path).map_err(io("creazione", path))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let mut header = Vec::with_capacity(68);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_len.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&40u32.to_le_bytes());
    header.extend_from_slice(&0xFFFEu16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
    header.extend_from_slice(&channels16.to_le_bytes());
    header.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&(SAMPLE_RATE * u32::from(block_align16)).to_le_bytes());
    header.extend_from_slice(&block_align16.to_le_bytes());
    header.extend_from_slice(&24u16.to_le_bytes());
    header.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    header.extend_from_slice(&24u16.to_le_bytes()); // wValidBitsPerSample
    header.extend_from_slice(&0u32.to_le_bytes()); // dwChannelMask: canali senza posizione
    header.extend_from_slice(&PCM_GUID);
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len32.to_le_bytes());
    let err = io("scrittura", path);
    let mut write_all = |b: &[u8]| w.write_all(b);
    let result = (|| -> std::io::Result<()> {
        write_all(&header)?;
        // Un blocco di frame alla volta da ogni traccia, poi interleaving a 24 bit.
        let mut readers: Vec<_> = (0..tracks).map(|t| audio.reader(t)).collect();
        let mut pcm = vec![vec![0.0f32; BLOCK * CHANNELS]; tracks];
        let mut out = Vec::with_capacity(BLOCK * block_align);
        let mut done = 0;
        while done < frames {
            let n = BLOCK.min(frames - done);
            for (r, buf) in readers.iter_mut().zip(pcm.iter_mut()) {
                let got = r.read(&mut buf[..n * CHANNELS]);
                buf[got * CHANNELS..n * CHANNELS].fill(0.0);
            }
            out.clear();
            for i in 0..n {
                for buf in &pcm {
                    for &x in &buf[i * CHANNELS..(i + 1) * CHANNELS] {
                        let v = (x.clamp(-1.0, 1.0) * S24_MAX).round() as i32;
                        out.extend_from_slice(&v.to_le_bytes()[..3]);
                    }
                }
            }
            write_all(&out)?;
            done += n;
        }
        if pad(data_len32 as usize) == 1 {
            write_all(&[0])?;
        }
        write_all(b"iXML")?;
        write_all(&(ixml.len() as u32).to_le_bytes())?;
        write_all(&ixml)?;
        if pad(ixml.len()) == 1 {
            write_all(&[0])?;
        }
        Ok(())
    })();
    result.and_then(|()| w.flush()).map_err(err)
}
