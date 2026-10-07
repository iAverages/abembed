//! Fixes MP4s whose timestamps are all inflated by a whole factor (2x and 4x seen on
//! TikTok downloads): video frames are labelled at a fraction of their real rate and
//! every AAC frame claims that many times its real length. Only the moov box is
//! edited, in place and without changing its size, so every byte offset into mdat
//! stays valid and can be streamed untouched.

use std::ops::Range;

pub enum Moov {
    Found(Range<usize>),
    /// The moov box runs past the buffer; at least this many bytes from the start are needed.
    NeedBytes(usize),
}

struct Bx {
    typ: [u8; 4],
    start: usize,
    payload: usize,
    end: usize,
}

pub fn find_moov(buf: &[u8]) -> Result<Moov, String> {
    let mut pos = 0;
    loop {
        if pos + 16 > buf.len() {
            return Ok(Moov::NeedBytes(pos + 16));
        }
        let b = read_box(buf, pos, usize::MAX)?;
        match &b.typ {
            b"moov" if b.end <= buf.len() => return Ok(Moov::Found(b.start..b.end)),
            b"moov" => return Ok(Moov::NeedBytes(b.end)),
            b"mdat" => return Err("moov is after mdat".into()),
            _ => pos = b.end,
        }
    }
}

/// Patches `moov` in place. Returns false, leaving it untouched, when the file doesn't
/// have the defect. On error `moov` may be half-edited, so callers should pass a copy.
pub fn patch(moov: &mut [u8]) -> Result<bool, String> {
    let root = read_box(moov, 0, moov.len())?;
    let traks: Vec<Bx> = children(moov, &root)?
        .into_iter()
        .filter(|b| &b.typ == b"trak")
        .collect();

    let mut audio = None;
    for trak in &traks {
        let hdlr = find(moov, trak, &[b"mdia", b"hdlr"])?;
        if &moov[hdlr.payload + 8..hdlr.payload + 12] == b"soun" {
            audio = Some(trak);
        }
    }
    let audio = audio.ok_or("no audio track")?;

    let mdhd = find(moov, audio, &[b"mdia", b"mdhd"])?;
    let stsz = find(moov, audio, &[b"mdia", b"minf", b"stbl", b"stsz"])?;
    let samples = u64::from(be_u32(moov, stsz.payload + 8));
    let frame = aac_frame_len(moov, audio, &mdhd)?;
    let real = samples * frame;
    let ratio = mdhd_duration(moov, &mdhd) as f64 / real as f64;
    let factor = ratio.round();
    if factor < 2.0 || (ratio - factor).abs() > 0.1 {
        return Ok(false);
    }
    let factor = factor as u32;

    let mvhd = find(moov, &root, &[b"mvhd"])?;
    shrink_duration(moov, &mvhd, factor);

    for trak in &traks {
        let is_audio = trak.start == audio.start;
        shrink_duration(moov, &find(moov, trak, &[b"tkhd"])?, factor);
        if let Ok(elst) = find(moov, trak, &[b"edts", b"elst"]) {
            shrink_edits(moov, &elst, is_audio, factor);
        }
        let mdhd = find(moov, trak, &[b"mdia", b"mdhd"])?;
        if is_audio {
            set_mdhd_duration(moov, &mdhd, real);
        } else {
            // Scaling up the timescale shrinks every video timestamp without touching stts/ctts.
            let ts = mdhd.payload + if moov[mdhd.payload] == 1 { 20 } else { 12 };
            let scaled = be_u32(moov, ts)
                .checked_mul(factor)
                .ok_or("video timescale overflows")?;
            set_u32(moov, ts, scaled);
        }
    }

    let stts = find(moov, audio, &[b"mdia", b"minf", b"stbl", b"stts"])?;
    rewrite_stts(moov, &stts, samples as u32, frame as u32)?;
    Ok(true)
}

/// Samples per AAC frame in the audio track's timescale. A frame is 1024 samples at the
/// codec's core rate, so it's 2048 when the timescale is the doubled HE-AAC (SBR) output rate.
fn aac_frame_len(moov: &[u8], audio: &Bx, mdhd: &Bx) -> Result<u64, String> {
    let timescale = be_u32(
        moov,
        mdhd.payload + if moov[mdhd.payload] == 1 { 20 } else { 12 },
    );
    let stsd = find(moov, audio, &[b"mdia", b"minf", b"stbl", b"stsd"])?;
    let entry = read_box(moov, stsd.payload + 8, stsd.end)?;
    if &entry.typ != b"mp4a" {
        return Err(format!(
            "audio is {}, not mp4a",
            String::from_utf8_lossy(&entry.typ)
        ));
    }
    if moov[entry.payload + 8..entry.payload + 10] != [0, 0] {
        return Err("unsupported mp4a sample entry version".into());
    }
    // A version 0 sound sample entry has 28 bytes of fields before its child boxes.
    let entry = Bx {
        payload: entry.payload + 28,
        ..entry
    };
    let esds = find(moov, &entry, &[b"esds"])?;
    let core = core_sample_rate(&moov[esds.payload + 4..esds.end])
        .ok_or("unreadable AAC config in esds")?;
    match timescale {
        t if t == core => Ok(1024),
        t if t == core * 2 => Ok(2048),
        t => Err(format!(
            "audio timescale {t} doesn't match AAC core rate {core}"
        )),
    }
}

/// Reads the core sample rate from the AudioSpecificConfig in an esds's ES_Descriptor.
fn core_sample_rate(d: &[u8]) -> Option<u32> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let mut p = descriptor(d, 0, 0x03)?;
    let flags = *d.get(p + 2)?;
    p += 3;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        p += 1 + usize::from(*d.get(p)?);
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    p = descriptor(d, p, 0x04)? + 13;
    p = descriptor(d, p, 0x05)?;

    let src = d.get(p..)?;
    let mut bits = [0; 8];
    bits[..src.len().min(8)].copy_from_slice(&src[..src.len().min(8)]);
    let bits = u64::from_be_bytes(bits);
    if bits >> 59 == 31 {
        return None;
    }
    match (bits >> 55) & 0xf {
        15 => Some(((bits >> 31) & 0xff_ffff) as u32),
        i => RATES.get(i as usize).copied(),
    }
}

/// Checks the descriptor at `p` has `tag` and returns where its payload starts.
fn descriptor(d: &[u8], mut p: usize, tag: u8) -> Option<usize> {
    if *d.get(p)? != tag {
        return None;
    }
    p += 1;
    for _ in 0..4 {
        p += 1;
        if *d.get(p - 1)? & 0x80 == 0 {
            break;
        }
    }
    Some(p)
}

/// Replaces the audio stts with a single run of fixed-length frames, filling the
/// bytes it no longer needs with a `free` box so nothing after it moves.
fn rewrite_stts(buf: &mut [u8], stts: &Bx, samples: u32, frame: u32) -> Result<(), String> {
    const LEN: usize = 24;
    let spare = (stts.end - stts.start)
        .checked_sub(LEN)
        .ok_or("stts is smaller than a single-entry stts")?;
    if spare != 0 && spare < 8 {
        return Err(format!(
            "stts has {spare} spare bytes, too few for a free box"
        ));
    }
    let s = stts.start;
    set_u32(buf, s, LEN as u32);
    buf[s + 4..s + 8].copy_from_slice(b"stts");
    set_u32(buf, s + 8, 0);
    set_u32(buf, s + 12, 1);
    set_u32(buf, s + 16, samples);
    set_u32(buf, s + 20, frame);
    if spare != 0 {
        set_u32(buf, s + LEN, spare as u32);
        buf[s + LEN + 4..s + LEN + 8].copy_from_slice(b"free");
        buf[s + LEN + 8..stts.end].fill(0);
    }
    Ok(())
}

fn shrink_edits(buf: &mut [u8], elst: &Bx, is_audio: bool, factor: u32) {
    let wide = buf[elst.payload] == 1;
    let count = be_u32(buf, elst.payload + 4) as usize;
    let size = if wide { 20 } else { 12 };
    for i in 0..count {
        let e = elst.payload + 8 + i * size;
        if wide {
            set_u64(buf, e, be_u64(buf, e) / u64::from(factor));
            // The audio edit's start skip is in audio samples, which we shrink; video
            // samples keep their units because only the video timescale changes.
            let t = be_u64(buf, e + 8) as i64;
            if is_audio && t > 0 {
                set_u64(buf, e + 8, (t / i64::from(factor)) as u64);
            }
        } else {
            set_u32(buf, e, be_u32(buf, e) / factor);
            let t = be_u32(buf, e + 4) as i32;
            if is_audio && t > 0 {
                set_u32(buf, e + 4, (t / factor as i32) as u32);
            }
        }
    }
}

/// Divides the duration of an mvhd or tkhd by `factor`.
fn shrink_duration(buf: &mut [u8], b: &Bx, factor: u32) {
    let wide = buf[b.payload] == 1;
    let off = b.payload
        + match (&b.typ, wide) {
            (b"tkhd", false) => 20,
            (b"tkhd", true) => 28,
            (_, false) => 16,
            (_, true) => 24,
        };
    if wide {
        set_u64(buf, off, be_u64(buf, off) / u64::from(factor));
    } else {
        set_u32(buf, off, be_u32(buf, off) / factor);
    }
}

fn mdhd_duration(buf: &[u8], mdhd: &Bx) -> u64 {
    if buf[mdhd.payload] == 1 {
        be_u64(buf, mdhd.payload + 24)
    } else {
        u64::from(be_u32(buf, mdhd.payload + 16))
    }
}

fn set_mdhd_duration(buf: &mut [u8], mdhd: &Bx, d: u64) {
    if buf[mdhd.payload] == 1 {
        set_u64(buf, mdhd.payload + 24, d);
    } else {
        set_u32(buf, mdhd.payload + 16, d as u32);
    }
}

fn find(buf: &[u8], parent: &Bx, path: &[&[u8; 4]]) -> Result<Bx, String> {
    let mut cur = children(buf, parent)?
        .into_iter()
        .find(|b| &b.typ == path[0])
        .ok_or_else(|| format!("missing {}", String::from_utf8_lossy(path[0])))?;
    for typ in &path[1..] {
        cur = children(buf, &cur)?
            .into_iter()
            .find(|b| &b.typ == *typ)
            .ok_or_else(|| format!("missing {}", String::from_utf8_lossy(*typ)))?;
    }
    Ok(cur)
}

fn children(buf: &[u8], parent: &Bx) -> Result<Vec<Bx>, String> {
    let mut out = Vec::new();
    let mut pos = parent.payload;
    while pos < parent.end {
        let b = read_box(buf, pos, parent.end)?;
        pos = b.end;
        out.push(b);
    }
    Ok(out)
}

fn read_box(buf: &[u8], pos: usize, limit: usize) -> Result<Bx, String> {
    if pos + 8 > buf.len() {
        return Err(format!("truncated box header at {pos}"));
    }
    let mut typ = [0; 4];
    typ.copy_from_slice(&buf[pos + 4..pos + 8]);
    let (size, header) = match be_u32(buf, pos) {
        0 => {
            return Err(format!(
                "{} box extends to end of file",
                String::from_utf8_lossy(&typ)
            ));
        }
        1 if pos + 16 > buf.len() => return Err(format!("truncated box header at {pos}")),
        1 => (be_u64(buf, pos + 8) as usize, 16),
        n => (n as usize, 8),
    };
    let end = pos
        .checked_add(size)
        .filter(|&e| size >= header && e <= limit);
    let end = end.ok_or_else(|| {
        format!(
            "bad {} box size {size} at {pos}",
            String::from_utf8_lossy(&typ)
        )
    })?;
    Ok(Bx {
        typ,
        start: pos,
        payload: pos + header,
        end,
    })
}

fn be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(buf[off..off + 4].try_into().unwrap())
}

fn be_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(buf[off..off + 8].try_into().unwrap())
}

fn set_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn set_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::core_sample_rate;

    const ESDS_LC_44100: [u8; 27] = [
        0x03, 0x19, 0x00, 0x01, 0x00, 0x04, 0x11, 0x40, 0x15, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x02, 0x12, 0x10, 0x06, 0x01, 0x02,
    ];

    #[test]
    fn reads_core_rate_from_lc_config() {
        assert_eq!(core_sample_rate(&ESDS_LC_44100), Some(44100));
    }

    #[test]
    fn reads_core_rate_from_implicit_sbr_config() {
        let mut esds = ESDS_LC_44100;
        esds[22..24].copy_from_slice(&[0x13, 0x90]);
        assert_eq!(core_sample_rate(&esds), Some(22050));
    }

    #[test]
    fn reads_core_rate_from_multibyte_lengths() {
        let mut esds = vec![
            0x03, 0x80, 0x80, 0x80, 0x22, 0x00, 0x01, 0x00, 0x04, 0x80, 0x80, 0x80, 0x14,
        ];
        esds.extend_from_slice(&ESDS_LC_44100[7..20]);
        esds.extend_from_slice(&[0x05, 0x80, 0x80, 0x80, 0x02, 0x11, 0x90]);
        assert_eq!(core_sample_rate(&esds), Some(48000));
    }

    #[test]
    fn rejects_truncated_config() {
        assert_eq!(core_sample_rate(&ESDS_LC_44100[..10]), None);
    }
}
