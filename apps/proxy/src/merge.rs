//! `/merge?video=URL&audio=URL`: serves a DASH video and audio stream as one MP4, see `remux`.

use futures_util::{StreamExt, future::try_join, stream};
use worker::{
    Context, Headers, Request, Response, ResponseBody, Url, console_error, js_sys,
    wasm_bindgen_futures::JsFuture, worker_sys,
};

use crate::{
    Error, bad_gateway, fetch_bytes, host_allowed, parse_range,
    remux::{self, Head, Kind, Part, Plan},
};

/// Covers ftyp + moov + sidx for typical streams; longer ones take a second fetch.
const HEAD_FETCH: u64 = 64 * 1024;
/// Streams whose sidx isn't within this many leading bytes are rejected.
const MAX_HEAD: u64 = 4 * 1024 * 1024;
/// Fragments fetched ahead of the one being written.
const PREFETCH: usize = 2;

pub async fn merge(
    req: &Request,
    allowed_hosts: &[String],
    ctx: &Context,
) -> Result<Response, Error> {
    let url = req.url().map_err(|e| (400, e.to_string()))?;
    let param = |name: &str| -> Result<Url, Error> {
        let (_, value) = url
            .query_pairs()
            .find(|(k, _)| k == name)
            .ok_or((400, format!("missing {name}")))?;
        let value = Url::parse(&value).map_err(|e| (400, e.to_string()))?;
        if !host_allowed(allowed_hosts, &value) {
            return Err((403, "host not allowed".into()));
        }
        Ok(value)
    };
    let (video, audio) = (param("video")?, param("audio")?);

    let (video_input, audio_input) = try_join(
        read_input(allowed_hosts, &video),
        read_input(allowed_hosts, &audio),
    )
    .await?;
    let plan = remux::plan(video_input, audio_input).map_err(bad_gateway)?;
    let total = plan.len;

    let range = req.headers().get("range").map_err(bad_gateway)?;
    let (start, end) = match &range {
        Some(v) => parse_range(v, total).ok_or((416, format!("bytes */{total}")))?,
        None => (0, total - 1),
    };

    let fixed = worker_sys::FixedLengthStream::new_big_int(js_sys::BigInt::from(end - start + 1))
        .map_err(|e| bad_gateway(format!("{e:?}")))?;
    let writable = fixed.writable();
    let allowed_hosts = allowed_hosts.to_vec();
    ctx.wait_until(async move {
        let parts = plan.parts(start, end);
        let sources = Sources {
            video,
            audio,
            allowed_hosts,
        };
        let res = async {
            let writer = writable.get_writer().map_err(|e| format!("{e:?}"))?;
            let mut pieces = stream::iter(parts)
                .map(|part| sources.read(&plan, part))
                .buffered(PREFETCH);
            while let Some(piece) = pieces.next().await {
                let piece = piece?;
                JsFuture::from(writer.write_with_chunk(&js_sys::Uint8Array::from(&piece[..])))
                    .await
                    .map_err(|e| format!("{e:?}"))?;
            }
            JsFuture::from(writer.close())
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok::<_, String>(())
        };
        if let Err(e) = res.await {
            console_error!("streaming merged {} failed: {e}", sources.video);
        }
    });

    let headers = Headers::new();
    headers
        .set("content-type", "video/mp4")
        .map_err(bad_gateway)?;
    headers.set("accept-ranges", "bytes").map_err(bad_gateway)?;
    let mut status = 200;
    if range.is_some() {
        status = 206;
        headers
            .set("content-range", &format!("bytes {start}-{end}/{total}"))
            .map_err(bad_gateway)?;
    }
    Ok(Response::from_body(ResponseBody::Stream(fixed.readable()))
        .map_err(bad_gateway)?
        .with_status(status)
        .with_headers(headers))
}

async fn read_input(allowed_hosts: &[String], url: &Url) -> Result<remux::Input, Error> {
    let (mut head, total) = fetch_bytes(allowed_hosts, url, 0, HEAD_FETCH - 1).await?;
    loop {
        match remux::read_head(&head).map_err(bad_gateway)? {
            Head::Found(input) => return Ok(*input),
            Head::NeedBytes(n) if n > head.len() && n as u64 <= total.min(MAX_HEAD) => {
                head = fetch_bytes(allowed_hosts, url, 0, n as u64 - 1).await?.0;
            }
            Head::NeedBytes(_) => {
                return Err(bad_gateway(format!(
                    "no sidx in the first {MAX_HEAD} bytes of {url}"
                )));
            }
        }
    }
}

struct Sources {
    video: Url,
    audio: Url,
    allowed_hosts: Vec<String>,
}

impl Sources {
    async fn read(&self, plan: &Plan, part: Part) -> Result<Vec<u8>, String> {
        let (index, range) = match part {
            Part::Header(range) => return Ok(plan.header[range].to_vec()),
            Part::Chunk(index, range) => (index, range),
        };
        let chunk = &plan.chunks[index];
        let (url, track) = match chunk.kind {
            Kind::Video => (&self.video, None),
            Kind::Audio => (&self.audio, Some(plan.audio_track)),
        };

        let (mut data, _) = fetch_bytes(
            &self.allowed_hosts,
            url,
            chunk.source,
            chunk.source + chunk.size - 1,
        )
        .await
        .map_err(|(_, e)| e)?;
        if data.len() as u64 != chunk.size {
            return Err(format!(
                "expected {} bytes of {url}, got {}",
                chunk.size,
                data.len()
            ));
        }
        remux::patch_chunk(
            &mut data,
            chunk.sequence,
            track,
            chunk.offset as i64 - chunk.source as i64,
        )?;

        data.truncate(range.end);
        data.drain(..range.start);
        Ok(data)
    }
}
