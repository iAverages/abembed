mod merge;
mod patch;
mod remux;

use worker::{
    Context, Env, Fetch, Headers, Request, RequestInit, RequestRedirect, Response, ResponseBody,
    Url, console_error, event, js_sys, wasm_bindgen_futures::JsFuture, web_sys, worker_sys,
};

/// Enough to cover ftyp + moov for typical short clips; longer files take a second fetch.
const HEAD_FETCH: u64 = 256 * 1024;
/// Files whose moov isn't within this many leading bytes (non-MP4, or moov at the end) pass through.
const MAX_HEAD: u64 = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

type Error = (u16, String);

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let allowed_hosts: Vec<String> = env
        .var("ALLOWED_HOSTS")?
        .to_string()
        .split(',')
        .map(str::to_owned)
        .collect();
    let res = match req.path().as_str() {
        "/video" => video(&req, &allowed_hosts, &ctx).await,
        "/merge" => merge::merge(&req, &allowed_hosts, &ctx).await,
        _ => return Response::error("not found", 404),
    };
    match res {
        Ok(res) => Ok(res),
        Err((status, msg)) => Response::error(msg, status),
    }
}

async fn video(req: &Request, allowed_hosts: &[String], ctx: &Context) -> Result<Response, Error> {
    let url = req
        .url()
        .map_err(|e| (400, e.to_string()))?
        .query_pairs()
        .find(|(k, _)| k == "url")
        .ok_or((400, "missing url".to_string()))?
        .1
        .into_owned();
    let url = Url::parse(&url).map_err(|e| (400, e.to_string()))?;
    if !host_allowed(allowed_hosts, &url) {
        return Err((403, "host not allowed".into()));
    }

    let (mut head, total) = fetch_bytes(allowed_hosts, &url, 0, HEAD_FETCH - 1).await?;
    let moov = loop {
        match patch::find_moov(&head) {
            Ok(patch::Moov::Found(r)) => break Ok(r),
            Ok(patch::Moov::NeedBytes(n)) if n as u64 <= total.min(MAX_HEAD) => {
                head = fetch_bytes(allowed_hosts, &url, 0, n as u64 - 1).await?.0;
            }
            Ok(patch::Moov::NeedBytes(_)) => {
                break Err(format!("no moov box in the first {MAX_HEAD} bytes"));
            }
            Err(e) => break Err(e),
        }
    };
    let patched = moov.and_then(|moov| {
        let mut fixed = head[moov.clone()].to_vec();
        Ok(patch::patch(&mut fixed)?.then_some((moov, fixed)))
    });

    // An empty head means everything is forwarded from upstream untouched.
    match patched {
        Ok(Some((moov, fixed))) => {
            head.truncate(moov.end);
            head[moov].copy_from_slice(&fixed);
        }
        Ok(None) => head.clear(),
        Err(e) => {
            console_error!("serving {url} unpatched: {e}");
            head.clear();
        }
    }

    let range = req.headers().get("range").map_err(bad_gateway)?;
    let (start, end) = match &range {
        Some(v) => parse_range(v, total).ok_or((416, format!("bytes */{total}")))?,
        None => (0, total - 1),
    };

    let head_len = head.len() as u64;
    let head_part =
        (start < head_len).then(|| head[start as usize..head_len.min(end + 1) as usize].to_vec());
    let tail = if end >= head_len {
        match ranged(allowed_hosts, &url, start.max(head_len), end)
            .await?
            .body()
        {
            ResponseBody::Stream(s) => Some(s.clone()),
            _ => return Err(bad_gateway("upstream sent no body")),
        }
    } else {
        None
    };
    let body = concat(ctx, end - start + 1, head_part, tail).map_err(bad_gateway)?;

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
    Ok(Response::from_body(ResponseBody::Stream(body))
        .map_err(bad_gateway)?
        .with_status(status)
        .with_headers(headers))
}

/// Writes `head` and then pipes `tail` into a FixedLengthStream, so the response keeps its
/// Content-Length and the tail is piped by the runtime instead of copied through wasm.
fn concat(
    ctx: &Context,
    len: u64,
    head: Option<Vec<u8>>,
    tail: Option<web_sys::ReadableStream>,
) -> worker::Result<web_sys::ReadableStream> {
    let fixed = worker_sys::FixedLengthStream::new_big_int(js_sys::BigInt::from(len))?;
    let writable = fixed.writable();
    ctx.wait_until(async move {
        let res = async {
            let writer = writable.get_writer()?;
            if let Some(head) = head {
                JsFuture::from(writer.write_with_chunk(&js_sys::Uint8Array::from(&head[..])))
                    .await?;
            }
            writer.release_lock();
            match tail {
                Some(tail) => JsFuture::from(tail.pipe_to(&writable)).await,
                None => JsFuture::from(writable.close()).await,
            }
        };
        if let Err(e) = res.await {
            console_error!("streaming response failed: {e:?}");
        }
    });
    Ok(fixed.readable())
}

/// Fetches an inclusive byte range, returning it with the upstream file's total size.
async fn fetch_bytes(
    allowed_hosts: &[String],
    url: &Url,
    start: u64,
    end: u64,
) -> Result<(Vec<u8>, u64), Error> {
    let mut res = ranged(allowed_hosts, url, start, end).await?;
    let total = res
        .headers()
        .get("content-range")
        .ok()
        .flatten()
        .and_then(|v| v.rsplit('/').next()?.parse().ok())
        .ok_or_else(|| bad_gateway("upstream sent no usable Content-Range"))?;
    let body = res.bytes().await.map_err(bad_gateway)?;
    Ok((body, total))
}

/// Follows redirects by hand so each hop can be checked against the allowed hosts.
async fn ranged(
    allowed_hosts: &[String],
    url: &Url,
    start: u64,
    end: u64,
) -> Result<Response, Error> {
    let mut url = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let headers = Headers::new();
        headers
            .set("range", &format!("bytes={start}-{end}"))
            .map_err(bad_gateway)?;
        let mut init = RequestInit::new();
        init.with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        let req = Request::new_with_init(url.as_str(), &init).map_err(bad_gateway)?;
        let res = Fetch::Request(req).send().await.map_err(bad_gateway)?;
        match res.status_code() {
            206 => return Ok(res),
            301 | 302 | 303 | 307 | 308 => {
                let location = res.headers().get("location").ok().flatten();
                url = location
                    .and_then(|l| url.join(&l).ok())
                    .filter(|u| host_allowed(allowed_hosts, u))
                    .ok_or_else(|| {
                        bad_gateway("upstream redirected to a host that isn't allowed")
                    })?;
            }
            s => {
                return Err(bad_gateway(format!(
                    "upstream answered a range request with {s}"
                )));
            }
        }
    }
    Err(bad_gateway("too many redirects"))
}

/// Parses a single `bytes=` range into inclusive offsets; multi-range requests aren't supported.
fn parse_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = match (a, b) {
        ("", n) => (total.checked_sub(n.parse().ok()?)?, total - 1),
        (a, "") => (a.parse().ok()?, total - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(total - 1)),
    };
    (start <= end).then_some((start, end))
}

fn host_allowed(allowed: &[String], url: &Url) -> bool {
    url.host_str().is_some_and(|h| {
        allowed
            .iter()
            .any(|a| h == a || h.ends_with(&format!(".{a}")))
    })
}

fn bad_gateway(e: impl ToString) -> Error {
    (502, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn parses_single_ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=500-5000", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=1000-1001", 1000), None);
        assert_eq!(parse_range("bytes=-2000", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
    }
}
