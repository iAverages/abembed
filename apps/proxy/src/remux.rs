//! Merges a DASH pair, a fragmented video MP4 and a fragmented audio MP4 that each start with
//! ftyp, moov and a single sidx (Facebook's on-demand profile), into one fragmented MP4. Only the
//! header is rebuilt: every fragment is copied from its source with its moof patched, so each
//! byte range of the output maps onto a byte range of one of the sources and can be streamed.

use std::{cmp::Ordering, ops::Range};

use mp4_atom::{Any, Codec, Colr, Decode, Encode, Ftyp, Header, Moov, SegmentReference, Sidx};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Video,
    Audio,
}

pub struct Input {
    ftyp: Ftyp,
    moov: Moov,
    sidx: Sidx,
    media_start: u64,
}

pub enum Head {
    Found(Box<Input>),
    /// The sidx runs past the buffer; at least this many bytes from the start are needed.
    NeedBytes(usize),
}

pub struct Plan {
    pub header: Vec<u8>,
    pub chunks: Vec<Chunk>,
    pub audio_track: u32,
    pub len: u64,
}

/// A sidx subsegment of one source, placed at `offset` in the output.
pub struct Chunk {
    pub kind: Kind,
    pub source: u64,
    pub size: u64,
    pub offset: u64,
    pub sequence: u32,
}

pub enum Part {
    Header(Range<usize>),
    Chunk(usize, Range<usize>),
}

struct Timed {
    kind: Kind,
    source: u64,
    size: u64,
    start: u64,
    timescale: u32,
}

/// Reads the ftyp, moov and sidx from the start of a stream.
pub fn read_head(buf: &[u8]) -> Result<Head, String> {
    let (mut ftyp, mut moov) = (None, None);
    let mut position = 0;

    loop {
        // a box header is at most 16 bytes
        if position + 16 > buf.len() {
            return Ok(Head::NeedBytes(position + 16));
        }
        let mut rest = &buf[position..];
        let header = Header::decode(&mut rest).map_err(|err| err.to_string())?;
        let size = header.size.ok_or("box extends to the end of the stream")?;
        let end = buf.len() - rest.len() + size;
        if end > buf.len() {
            return Ok(Head::NeedBytes(end));
        }

        let atom = Any::decode(&mut &buf[position..end]).map_err(|err| err.to_string())?;
        match atom {
            Any::Ftyp(atom) => ftyp = Some(atom),
            Any::Moov(atom) => moov = Some(atom),
            Any::Sidx(sidx) => {
                return Ok(Head::Found(Box::new(Input {
                    ftyp: ftyp.ok_or("stream has no ftyp before its sidx")?,
                    moov: moov.ok_or("stream has no moov before its sidx")?,
                    media_start: end as u64 + sidx.first_offset,
                    sidx,
                })));
            }
            Any::Moof(_) | Any::Mdat(_) => return Err("stream has no sidx before its media".into()),
            _ => {}
        }
        position = end;
    }
}

pub fn plan(video: Input, audio: Input) -> Result<Plan, String> {
    let [video_trak] = video.moov.trak.as_slice() else {
        return Err("video stream should have exactly one track".into());
    };
    let video_track = video_trak.tkhd.track_id;
    let audio_track = video_track + 1;

    let mut timed = timed_chunks(Kind::Video, &video)?;
    timed.extend(timed_chunks(Kind::Audio, &audio)?);
    timed.sort_by(compare_start);

    // one sidx per track so a demuxer can find each track's fragments when seeking
    let audio_sidx = track_sidx(Kind::Audio, audio_track, &audio.sidx, &timed, 0)?;
    let mut audio_sidx_bytes = Vec::new();
    audio_sidx
        .encode(&mut audio_sidx_bytes)
        .map_err(|err| err.to_string())?;
    let video_sidx = track_sidx(
        Kind::Video,
        video_track,
        &video.sidx,
        &timed,
        audio_sidx_bytes.len() as u64,
    )?;
    let mut moov = merge_moov(video.moov, audio.moov, audio_track)?;
    fix_colour_primaries(&mut moov);

    let mut header = Vec::new();
    video
        .ftyp
        .encode(&mut header)
        .map_err(|err| err.to_string())?;
    moov.encode(&mut header).map_err(|err| err.to_string())?;
    video_sidx
        .encode(&mut header)
        .map_err(|err| err.to_string())?;
    header.extend(audio_sidx_bytes);

    let mut offset = header.len() as u64;
    let chunks = timed
        .into_iter()
        .enumerate()
        .map(|(position, chunk)| {
            let placed = Chunk {
                kind: chunk.kind,
                source: chunk.source,
                size: chunk.size,
                offset,
                sequence: position as u32 + 1,
            };
            offset += chunk.size;
            placed
        })
        .collect();

    Ok(Plan {
        header,
        chunks,
        audio_track,
        len: offset,
    })
}

impl Plan {
    /// The pieces covering the inclusive output range `start..=end`.
    pub fn parts(&self, start: u64, end: u64) -> Vec<Part> {
        let mut parts = Vec::new();
        let header_len = self.header.len() as u64;
        if start < header_len {
            parts.push(Part::Header(
                start as usize..(end + 1).min(header_len) as usize,
            ));
        }

        for (index, chunk) in self.chunks.iter().enumerate() {
            let chunk_end = chunk.offset + chunk.size;
            if chunk_end <= start || chunk.offset > end {
                continue;
            }
            let from = start.max(chunk.offset) - chunk.offset;
            let to = (end + 1).min(chunk_end) - chunk.offset;
            parts.push(Part::Chunk(index, from as usize..to as usize));
        }

        parts
    }
}

fn timed_chunks(kind: Kind, input: &Input) -> Result<Vec<Timed>, String> {
    let mut source = input.media_start;
    let mut start = input.sidx.earliest_presentation_time;
    let mut chunks = Vec::new();

    for reference in &input.sidx.references {
        if reference.reference_type {
            return Err("nested sidx references aren't supported".into());
        }
        chunks.push(Timed {
            kind,
            source,
            size: u64::from(reference.referenced_size),
            start,
            timescale: input.sidx.timescale,
        });
        source += u64::from(reference.referenced_size);
        start += u64::from(reference.subsegment_duration);
    }

    Ok(chunks)
}

/// A sidx for one track, `gap` bytes before the media, whose references each cover one of the
/// track's subsegments plus the other track's chunks placed after it.
fn track_sidx(
    kind: Kind,
    reference_id: u32,
    source: &Sidx,
    timed: &[Timed],
    gap: u64,
) -> Result<Sidx, String> {
    let leading: u64 = timed
        .iter()
        .take_while(|chunk| chunk.kind != kind)
        .map(|chunk| chunk.size)
        .sum();
    let mut sizes: Vec<u64> = Vec::new();
    for chunk in timed {
        match sizes.last_mut() {
            Some(size) if chunk.kind != kind => *size += chunk.size,
            None if chunk.kind != kind => {}
            _ => sizes.push(chunk.size),
        }
    }
    if sizes.is_empty() {
        return Err("stream has no segments".into());
    }

    let references = source
        .references
        .iter()
        .zip(sizes)
        .map(|(reference, size)| {
            Ok(SegmentReference {
                referenced_size: to_u32(size)?,
                ..reference.clone()
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(Sidx {
        reference_id,
        timescale: source.timescale,
        earliest_presentation_time: source.earliest_presentation_time,
        first_offset: gap + leading,
        references,
    })
}

/// Orders by start time across both timescales, video first on ties.
fn compare_start(a: &Timed, b: &Timed) -> Ordering {
    let a_time = u128::from(a.start) * u128::from(b.timescale);
    let b_time = u128::from(b.start) * u128::from(a.timescale);

    a_time
        .cmp(&b_time)
        .then_with(|| (a.kind == Kind::Audio).cmp(&(b.kind == Kind::Audio)))
}

fn merge_moov(mut video: Moov, audio: Moov, audio_track: u32) -> Result<Moov, String> {
    let video_timescale = u128::from(video.mvhd.timescale);
    let audio_timescale = u128::from(audio.mvhd.timescale);
    let rescale = |duration: u64| (u128::from(duration) * video_timescale / audio_timescale) as u64;

    let [mut trak] =
        <[_; 1]>::try_from(audio.trak).map_err(|_| "audio stream should have exactly one track")?;
    trak.tkhd.track_id = audio_track;
    trak.tkhd.duration = rescale(trak.tkhd.duration);
    if let Some(elst) = trak.edts.as_mut().and_then(|edts| edts.elst.as_mut()) {
        for entry in &mut elst.entries {
            entry.segment_duration = rescale(entry.segment_duration);
        }
    }

    let audio_mvex = audio.mvex.ok_or("audio stream isn't fragmented")?;
    let mut trex = audio_mvex
        .trex
        .into_iter()
        .next()
        .ok_or("audio stream has no trex")?;
    trex.track_id = audio_track;

    let mvex = video.mvex.as_mut().ok_or("video stream isn't fragmented")?;
    mvex.trex.push(trex);
    if let (Some(mehd), Some(audio_mehd)) = (mvex.mehd.as_mut(), audio_mvex.mehd) {
        mehd.fragment_duration = mehd
            .fragment_duration
            .max(rescale(audio_mehd.fragment_duration));
    }

    video.mvhd.duration = video.mvhd.duration.max(rescale(audio.mvhd.duration));
    video.mvhd.next_track_id = audio_track + 1;
    video.trak.push(trak);

    Ok(video)
}

/// Chrome drops a video track whose colr has the reserved primaries value 0 and plays the file
/// as audio only, so it's marked unspecified instead.
fn fix_colour_primaries(moov: &mut Moov) {
    const UNSPECIFIED: u16 = 2;

    for trak in &mut moov.trak {
        for codec in &mut trak.mdia.minf.stbl.stsd.codecs {
            let colr = match codec {
                Codec::Avc1(entry) => &mut entry.colr,
                Codec::Avc3(entry) => &mut entry.colr,
                Codec::Hev1(entry) => &mut entry.colr,
                Codec::Hvc1(entry) => &mut entry.colr,
                Codec::Vp08(entry) => &mut entry.colr,
                Codec::Vp09(entry) => &mut entry.colr,
                Codec::Av01(entry) => &mut entry.colr,
                _ => continue,
            };
            if let Some(
                Colr::Nclx {
                    colour_primaries, ..
                }
                | Colr::Nclc {
                    colour_primaries, ..
                },
            ) = colr
                && *colour_primaries == 0
            {
                *colour_primaries = UNSPECIFIED;
            }
        }
    }
}

/// Renumbers a fragment for its place in the output: the mfhd sequence number, the audio track
/// id, and any absolute base data offset in its tfhd, which moves by `offset_delta`.
pub fn patch_chunk(
    data: &mut [u8],
    sequence: u32,
    track: Option<u32>,
    offset_delta: i64,
) -> Result<(), String> {
    for (kind, moof) in boxes(data, 0..data.len())? {
        if &kind != b"moof" {
            continue;
        }
        for (kind, body) in boxes(data, moof)? {
            match &kind {
                b"mfhd" => write_u32(data, body.start + 4, sequence),
                b"traf" => {
                    for (kind, tfhd) in boxes(data, body)? {
                        if &kind != b"tfhd" {
                            continue;
                        }
                        if let Some(track) = track {
                            write_u32(data, tfhd.start + 4, track);
                        }
                        let has_base_data_offset = data[tfhd.start + 3] & 1 == 1;
                        if has_base_data_offset {
                            let at = tfhd.start + 8;
                            let base = u64::from_be_bytes(data[at..at + 8].try_into().unwrap());
                            let moved = base
                                .checked_add_signed(offset_delta)
                                .ok_or("bad base data offset")?;
                            data[at..at + 8].copy_from_slice(&moved.to_be_bytes());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Child boxes of `range` as their type and body range.
type Boxes = Vec<([u8; 4], Range<usize>)>;

fn boxes(data: &[u8], range: Range<usize>) -> Result<Boxes, String> {
    let mut found = Vec::new();
    let mut position = range.start;

    while position < range.end {
        let mut buf = &data[position..range.end];
        let header = Header::decode(&mut buf).map_err(|err| err.to_string())?;
        let body_start = range.end - buf.len();
        let body_end = header.size.map_or(range.end, |size| body_start + size);
        if body_end > range.end {
            return Err(format!("box {} runs past its parent", header.kind));
        }
        found.push((header.kind.into(), body_start..body_end));
        position = body_end;
    }

    Ok(found)
}

fn write_u32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn to_u32(size: u64) -> Result<u32, String> {
    u32::try_from(size).map_err(|_| "segment is too large for a sidx".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ftyp + moov + sidx from the start of a real facebook dash pair
    const VIDEO_HEAD: &[u8] = include_bytes!("fixtures/dash-video-head.mp4");
    const AUDIO_HEAD: &[u8] = include_bytes!("fixtures/dash-audio-head.mp4");

    fn input(head: &[u8]) -> Input {
        match read_head(head).unwrap() {
            Head::Found(input) => *input,
            Head::NeedBytes(n) => panic!("needs {n} bytes"),
        }
    }

    fn merged_moov(plan: &Plan) -> (Moov, Vec<Sidx>) {
        let mut buf = &plan.header[..];
        let (mut moov, mut sidx) = (None, Vec::new());
        while let Some(atom) = mp4_atom::DecodeMaybe::decode_maybe(&mut buf).unwrap() {
            match atom {
                Any::Moov(atom) => moov = Some(atom),
                Any::Sidx(atom) => sidx.push(atom),
                _ => {}
            }
        }
        (moov.unwrap(), sidx)
    }

    #[test]
    fn asks_for_more_bytes_until_the_sidx() {
        assert!(matches!(
            read_head(&VIDEO_HEAD[..8]),
            Ok(Head::NeedBytes(16))
        ));
        assert!(matches!(
            read_head(&VIDEO_HEAD[..500]),
            Ok(Head::NeedBytes(826))
        ));
        assert!(matches!(
            read_head(&VIDEO_HEAD[..850]),
            Ok(Head::NeedBytes(906))
        ));
        assert_eq!(input(VIDEO_HEAD).media_start, 906);
        assert_eq!(input(AUDIO_HEAD).media_start, 976);
    }

    #[test]
    fn plans_an_interleaved_two_track_file() {
        let (video, audio) = (input(VIDEO_HEAD), input(AUDIO_HEAD));
        let (video_segments, audio_segments) =
            (video.sidx.references.len(), audio.sidx.references.len());
        let plan = plan(video, audio).unwrap();
        let (moov, sidx) = merged_moov(&plan);

        assert_eq!(plan.audio_track, 2);
        assert_eq!(
            moov.trak
                .iter()
                .map(|trak| trak.tkhd.track_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(moov.mvex.unwrap().trex.len(), 2);
        assert_eq!(moov.mvhd.next_track_id, 3);
        assert_eq!(plan.chunks.len(), video_segments + audio_segments);
        assert_eq!(
            sidx.iter()
                .map(|sidx| (sidx.reference_id, sidx.references.len()))
                .collect::<Vec<_>>(),
            [(1, video_segments), (2, audio_segments)]
        );

        // each sidx starts at its track's first chunk and covers every byte after it
        let mut audio_sidx = Vec::new();
        sidx[1].encode(&mut audio_sidx).unwrap();
        let first_audio = plan
            .chunks
            .iter()
            .find(|chunk| chunk.kind == Kind::Audio)
            .unwrap();
        let media_len = plan.len - plan.header.len() as u64;
        let referenced = |sidx: &Sidx| -> u64 {
            sidx.references
                .iter()
                .map(|reference| u64::from(reference.referenced_size))
                .sum()
        };
        assert_eq!(sidx[0].first_offset, audio_sidx.len() as u64);
        assert_eq!(referenced(&sidx[0]), media_len);
        assert_eq!(
            sidx[1].first_offset,
            first_audio.offset - plan.header.len() as u64
        );
        assert_eq!(referenced(&sidx[1]), plan.len - first_audio.offset);
        let mut offset = plan.header.len() as u64;
        for (position, chunk) in plan.chunks.iter().enumerate() {
            assert_eq!(chunk.offset, offset);
            assert_eq!(chunk.sequence, position as u32 + 1);
            offset += chunk.size;
        }
        assert_eq!(plan.chunks[0].kind, Kind::Video);
        assert_eq!(plan.chunks[0].source, 906);
    }

    #[test]
    fn marks_reserved_colour_primaries_unspecified() {
        let mut video_head = VIDEO_HEAD.to_vec();
        let colr = video_head
            .windows(4)
            .position(|window| window == b"colr")
            .unwrap();
        assert_eq!(&video_head[colr + 4..colr + 8], b"nclx");
        video_head[colr + 8..colr + 10].copy_from_slice(&0u16.to_be_bytes());

        let plan = plan(input(&video_head), input(AUDIO_HEAD)).unwrap();
        let (moov, _) = merged_moov(&plan);
        let Codec::Av01(av01) = &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] else {
            panic!("expected av01");
        };

        assert!(matches!(
            av01.colr,
            Some(Colr::Nclx {
                colour_primaries: 2,
                ..
            })
        ));
    }

    #[test]
    fn splits_ranges_across_the_header_and_chunks() {
        let plan = plan(input(VIDEO_HEAD), input(AUDIO_HEAD)).unwrap();
        let header_len = plan.header.len();
        let first = &plan.chunks[0];

        let parts = plan.parts(10, first.offset + 99);
        assert!(
            matches!(&parts[..], [Part::Header(header), Part::Chunk(0, chunk)]
            if *header == (10..header_len) && *chunk == (0..100))
        );

        let parts = plan.parts(first.offset + 5, plan.len - 1);
        assert!(matches!(&parts[0], Part::Chunk(0, chunk) if chunk.start == 5));
        assert_eq!(parts.len(), plan.chunks.len());
    }

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut atom = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        atom.extend_from_slice(kind);
        atom.extend_from_slice(body);
        atom
    }

    #[test]
    fn patches_sequence_track_and_base_offset() {
        let mfhd = atom(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 7]);
        let mut tfhd_body = vec![0, 0, 0, 1];
        tfhd_body.extend_from_slice(&1u32.to_be_bytes());
        tfhd_body.extend_from_slice(&1000u64.to_be_bytes());
        let traf = atom(b"traf", &atom(b"tfhd", &tfhd_body));
        let mut chunk = atom(b"moof", &[mfhd, traf].concat());
        chunk.extend(atom(b"mdat", &[1, 2, 3]));

        patch_chunk(&mut chunk, 42, Some(2), 500).unwrap();

        let read = |at: usize, len: usize| chunk[at..at + len].to_vec();
        // moof(8) mfhd(8 + version) sequence
        assert_eq!(read(20, 4), 42u32.to_be_bytes());
        // traf(8) tfhd(8 + flags) at 32: track id, then base data offset
        assert_eq!(read(44, 4), 2u32.to_be_bytes());
        assert_eq!(read(48, 8), 1500u64.to_be_bytes());
        assert_eq!(&chunk[chunk.len() - 3..], [1, 2, 3]);
    }
}
