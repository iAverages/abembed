# Reddit source

Fetch `embed.reddit.com` through the provider workers (www.reddit.com blocks server IPs),
`www.reddit.com/oembed` only as a title/author fallback. Lives in
`apps/abembed/src/sources/reddit/{mod.rs,page.rs,fixtures.rs}`, same shape as tiktok.

## Stage 1: Source, routes, post parsing
**Goal**: `RedditSource { subreddit, post_id, comment_id: Option }` parsing image, gallery,
video, text and gif posts from `https://embed.reddit.com/r/{sub}/comments/{id}/`.
- Routes (path mirrors reddit so `reddit.com` → our domain works):
  `/reddit/r/{sub}/comments/{id}`, `.../{id}/{slug}`, plus `/media/{index}` and `/api/reddit/...`
- `cache_key`: `embed:v1:reddit:{post_id}` (`:{comment_id}` for comments), subreddit is not part
  of identity
- Parse with `scraper`:
  - post info: `shreddit-screenview-data[data]` JSON → `post.type`, `post.url`, `post.nsfw`
  - title: `h1` or `shreddit-embed-title`
  - body: `#t3_{id}-post-rtjson-content` text
  - author: first `a[href*="/user/"]` in header
  - upvotes: `faceplate-number[number]` → `Stats.likes`
  - comments: `View (\d+) comments?`, "View more on Reddit" → 0 → `Stats.comments`
  - image: `post.url` (i.redd.it)
  - gallery: `gallery-carousel img` `preview.redd.it/...-v0-{ID}.{ext}` → `i.redd.it/{ID}.{ext}`
  - video: `shreddit-player[packaged-media-json]` highest `playbackMp4s.permutations` (has audio,
    width/height)
- Title rendering: description = `**{title}**\n{body}` (renderer has no title slot)
**Success Criteria**: each type parses into the right `EmbedPost`
**Tests**: trimmed real-page fixtures per type (image, gallery, video, text), `from_url`
accepts/rejects links like `identifies_tiktok_post_links`
**Status**: Complete (cache key includes the lowercased subreddit, a wrong-subreddit embed has no
title or media and must not poison the real post's entry)

## Stage 2: Text-only posts
**Goal**: posts with no media render instead of being treated as gated.
- `html.rs`: skip the media gallery component when `media` is empty
- `mod.rs` `api`: only 403 empty-media posts for sources that opt in with
  `Source::GATED_WHEN_EMPTY` (instagram)
**Success Criteria**: askscience text post embeds with title + body, api returns 200
**Tests**: empty-media reddit post renders component without gallery; instagram gated
behaviour unchanged
**Status**: Complete

## Stage 3: Comments
**Goal**: `/reddit/r/{sub}/comments/{id}/comment/{cid}` and `.../{id}/{slug}/{cid}`.
- Fetch `embed.reddit.com/r/{sub}/comments/{id}/comment/{cid}/`
- body: `#t1_{cid}-post-rtjson-content`; score: `comment.score` from screenview JSON
- media: `figure img` preview → `i.redd.it/{ID}.{ext}`; gif `shreddit-player[gif]` →
  `i.redd.it/{ID}.gif`
- video (`content_type: video`, no player rendered): id from `[id$="-aspect-ratio"]`, highest
  video + audio `BaseURL` from `v.redd.it/{ID}/DASHPlaylist.mpd` (CMAF_*, unsigned, no expiry),
  video is silent so `merged_video_url(video, audio)`; add `v.redd.it` to proxy `ALLOWED_HOSTS`
- replies: same `View N comments` regex
- parent title: fetch post embed concurrently, title only, failure non-fatal
**Success Criteria**: text, image, gif, video comments from the research links embed
**Tests**: fixtures for text, image, gif, video comment + DASH playlist
**Status**: Complete (parent title is fetched after the comment, not concurrently, it is cached)

## Stage 4: "Not supported post" fallback
**Goal**: posts like `osugame/1xjgqr2` still embed.
- page says "Not supported post" / `post.type == "link"` with no title
- media: `post.url` when host is `i.redd.it`
- title/author: `https://www.reddit.com/oembed?url={source_url}` → `title`, `author_name`;
  if that fails, title from the url slug
**Success Criteria**: 1xjgqr2 embeds with image and title
**Tests**: not-supported fixture + oembed json fixture, slug fallback
**Status**: Complete (oembed author only used for posts, it is the post author)

## Stage 4b: Short links (best effort)
**Goal**: `/reddit/{id}` (redd.it) and `/reddit/r/{sub}/s/{code}` resolve to a full post url.
- same pattern as tiktok `resolve_short_link`: `fetch_redirect` + `embed-alias:v1:reddit:` cache
- redd.it needs two hops: `redd.it/{id}` → `www.reddit.com/comments/{id}` →
  `/r/{sub}/comments/{id}/{slug}/`. Hop 2 is www.reddit.com, works in a real browser, 403 from
  curl locally, unknown from the provider
- if www blocks the provider, these return 502, everything else unaffected
**Success Criteria**: verified from the deployed provider, or dropped
**Tests**: `from_url` accepts the resolved location, rejects non-reddit redirects
**Status**: Not Started

## Stage 5: Wiring + deploy check
**Goal**: reachable in prod.
- `lib.rs` router + `PLATFORM_HOSTS`, `wrangler.toml` routes, `openapi.json`, README
- Verify from the deployed provider: embed.reddit.com, i.redd.it, packaged-media.redd.it,
  www.reddit.com/oembed
**Success Criteria**: all research links embed in Discord
**Status**: In Progress (wiring done and every research link checked against a local worker +
provider, prod deploy check pending)

## Stage 6: Hosted provider service
**Goal**: `apps/hosted-provider`, a self-hosted stand-in for the provider worker. reddit 403s every
cloudflare provider (checked with tail on provider-a, full browser UA too) but serves home servers.
- axum + reqwest like gif-service, listens on :3003
- `POST /fetch` json `{ url, headers: [[name, value]], follow_redirects }` → upstream status,
  `location`, `set-cookie`, `content-type` and body passed straight back
- `Authorization: Bearer $HOSTED_PROVIDER_TOKEN`, 401 otherwise; `GET /health`
**Success Criteria**: curl with the token fetches embed.reddit.com from a home server, without it 401
**Tests**: rejects missing/wrong token, rejects non-http urls, passes status/location/body through
from a local upstream with redirects followed and not followed
**Status**: Complete (reddit 403s reqwest over http/1.1 even from allowed ips, the `http2`
feature fixes it)

## Stage 7: abembed uses hosted providers for reddit
**Goal**: reddit fetches go to the hosted providers, every other source unchanged.
- secrets `HOSTED_PROVIDERS` (comma separated base urls, keeps home server hosts out of the public
  repo) and `HOSTED_PROVIDER_TOKEN`
- `Providers` holds named providers, cloudflare ones named by region, hosted ones by host;
  `Source::HOSTED` picks which set `load_post` builds
- workers can only fetch hostnames on standard ports, so hosted providers need a domain + https
**Success Criteria**: local worker with `.dev.vars` pointing at a local hosted provider embeds reddit
**Tests**: hosted provider list parsing
**Status**: In Progress (code kept, verified locally: worker → local hosted provider embeds video,
text and comment. reddit is back on the normal providers while we debug their 403, turn on with
`const HOSTED: bool = true` once decided)

## Stage 8: Packaging
**Goal**: hosted provider is buildable and runnable anywhere.
- `apps/hosted-provider/nix/packages.nix` package (no docker image yet), flake checks/packages
- `nixosModules.hosted-provider`: systemd service, `listen` (default 127.0.0.1:3003),
  `environmentFile` with the token
**Success Criteria**: `nix build .#hostedProvider`, module evaluates in a nixos system
**Status**: Complete

## Risks / open
- packaged-media `e=` expiry looked bucketed (~4.8h left when seen); cache TTL is 1h, check `e`
  is never under that
- oembed needs a full browser User-Agent, plain `Mozilla/5.0` gets a 403 "Blocked" page
  (embed.reddit.com accepts any UA)
- short links (stage 4b) can't skip www.reddit.com: oembed rejects urls without the subreddit
  (400), embed page with a wrong subreddit has no title/media
- untested: gif posts, external link posts, nsfw gallery/video, long text truncation
- nsfw posts are "Not supported post" from UK IPs (Online Safety Act age gate), they render fully
  elsewhere. Provider regions are us/eu-west-1/ap so prod should be fine, confirm in the deploy
  check. Test nsfw locally with a non-UK VPN; the fallback covers single images if one is gated
