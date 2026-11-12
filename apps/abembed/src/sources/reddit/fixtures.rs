// trimmed from real embed.reddit.com pages
use super::RedditSource;
use crate::sources::html::escape_html;
use serde_json::{json, Value};

pub const SUBREDDIT: &str = "osugame";
pub const POST_ID: &str = "1xm08ih";
pub const COMMENT_ID: &str = "19roziotq9zur";
pub const AUTHOR: &str = "Novapierce_";
pub const PACKAGED_1080: &str = "https://packaged-media.redd.it/wyq5lk4ow7zh1/pb/m2-res_1080p.mp4?m=DASHPlaylist.mpd&v=1&e=1793667600&s=12fa";
pub const OEMBED: &str = r#"{"author_name":"somecallmehero","html":"<blockquote></blockquote>","provider_name":"reddit","provider_url":"https://www.reddit.com","title":"Pippi x Furina Yuri","type":"rich","height":500}"#;
pub const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/"><category term="osugame" label="r/osugame"/><id>/r/osugame/comments/1xm08ih/.rss</id><title>Pippi x Furina Yuri : osugame</title><entry><author><name>/u/somecallmehero</name><uri>https://www.reddit.com/user/somecallmehero</uri></author><content type="html">&lt;table&gt; &lt;tr&gt;&lt;td&gt; &lt;a href=&quot;https://www.reddit.com/r/osugame/comments/1xm08ih/pippi_x_furina_yuri/&quot;&gt; &lt;img src=&quot;https://external-preview.redd.it/aHZqczRt.png?width=640&amp;amp;crop=smart&quot; alt=&quot;Pippi x Furina Yuri&quot; /&gt; &lt;/a&gt; &lt;/td&gt;&lt;td&gt; &lt;!-- SC_OFF --&gt;&lt;div class=&quot;md&quot;&gt;&lt;p&gt;For me I find that it&amp;#39;s a &amp;quot;press Q to panic&amp;quot; button. &lt;/p&gt; &lt;p&gt;Here in this clip you can see why. &lt;/p&gt; &lt;/div&gt;&lt;!-- SC_ON --&gt; &amp;#32; submitted by &amp;#32; &lt;a href=&quot;https://www.reddit.com/user/somecallmehero&quot;&gt; /u/somecallmehero &lt;/a&gt; &lt;br/&gt; &lt;span&gt;&lt;a href=&quot;https://v.redd.it/4fl9orxrh61i1&quot;&gt;[link]&lt;/a&gt;&lt;/span&gt; &lt;/td&gt;&lt;/tr&gt;&lt;/table&gt;</content><id>t3_1xm08ih</id><link href="https://www.reddit.com/r/osugame/comments/1xm08ih/pippi_x_furina_yuri/" /><title>Pippi x Furina Yuri</title></entry><entry><author><name>/u/zepol_xela</name><uri>https://www.reddit.com/user/zepol_xela</uri></author><content type="html">&lt;!-- SC_OFF --&gt;&lt;div class=&quot;md&quot;&gt;&lt;p&gt;Furina in r/osugame? damn&lt;/p&gt; &lt;/div&gt;&lt;!-- SC_ON --&gt;</content><id>t1_19roziotq9zur</id><link href="https://www.reddit.com/r/osugame/comments/1xm08ih/pippi_x_furina_yuri/19roziotq9zur/"/><title>/u/zepol_xela on Pippi x Furina Yuri</title></entry></feed>"#;

pub fn source() -> RedditSource {
    RedditSource::new(SUBREDDIT, POST_ID, None, None).unwrap()
}

pub fn comment_source() -> RedditSource {
    RedditSource::new(SUBREDDIT, POST_ID, None, Some(COMMENT_ID)).unwrap()
}

fn page(data: Value, body: &str) -> String {
    format!(
        r#"<!doctype html><html><body><shreddit-screenview-data data="{}"></shreddit-screenview-data>{body}</body></html>"#,
        escape_html(&data.to_string())
    )
}

fn post_page(kind: &str, url: &str, body: &str) -> String {
    page(
        json!({
            "post": { "id": format!("t3_{POST_ID}"), "url": url, "nsfw": false, "type": kind },
            "subreddit": { "name": SUBREDDIT },
        }),
        body,
    )
}

fn comment_page(content_type: Option<&str>, body: &str) -> String {
    page(
        json!({
            "comment": { "id": format!("t1_{COMMENT_ID}"), "post_id": format!("t3_{POST_ID}"), "score": 1372, "content_type": content_type },
            "subreddit": { "name": SUBREDDIT },
        }),
        body,
    )
}

const POST_HEADER: &str = r#"<div class="text-12 leading-4 text-neutral-content-weak line-clamp-1"> <span class="embed-s:hidden"> Posted by </span> <a class="text-secondary-weak hover:text-secondary-hover hover:underline" href="https://www.reddit.com/user/Novapierce_/?utm_source=embedv2&amp;utm_medium=post_embed&amp;utm_content=header" target="_blank">Novapierce_</a> · <faceplate-timeago ts="2026-11-01T18:09:05.751000+0000" short></faceplate-timeago> </div>"#;

const COMMENT_HEADER: &str = r#"<a href="https://www.reddit.com/user/zepol_xela/?utm_source=embedv2&amp;utm_medium=comment_embed&amp;utm_content=header" data-testid="user-icon"> <div class="rounded-full"><img src="https://styles.redditmedia.com/t5_avatar.png" alt=""></div> </a> <faceplate-tracker source="global" action="click" noun="embed"> <a href="https://www.reddit.com/user/zepol_xela/?utm_source=embedv2&amp;utm_medium=comment_embed&amp;utm_content=header" data-testid="user-name" target="_blank"> zepol_xela </a> </faceplate-tracker> <span> commented on <a rpl data-testid="post-link" href="https://www.reddit.com/r/ZZZ_Official/comments/1xg46l7/without_context_it_just_looks_like_shes_surprised/">post</a> </span>"#;

fn footer(upvotes: u64, comments: &str) -> String {
    format!(
        r#"<a rpl data-testid="upvote" href="https://www.reddit.com/r/osugame/comments/1xm08ih/"> <span class="flex items-center gap-x-2xs"> <span class="hover:text-secondary-onBackground hover:underline"><faceplate-number number="{upvotes}" pretty></faceplate-number> upvotes</span> </span> </a> <a rpl data-testid="cta" href="https://www.reddit.com/r/osugame/comments/1xm08ih/"><span class="flex items-center justify-center"> <span class="flex items-center gap-xs">{comments}</span> </span> </a>"#
    )
}

fn title(title: &str) -> String {
    format!(r#"<h1 class="line-clamp-3 m-0 text-[16px] leading-[20px] font-bold"> {title} </h1>"#)
}

pub fn image_post_page() -> String {
    post_page(
        "image",
        "https://i.redd.it/v1eq2vzfk1zh1.jpeg",
        &[
            POST_HEADER,
            &title("I don&#39;t like this but it&#39;s right"),
            r#"<img alt="" class=" h-full w-full object-contain relative" loading="lazy" src="https://preview.redd.it/i-dont-like-this-v0-v1eq2vzfk1zh1.jpeg?width=640&amp;crop=smart&amp;auto=webp&amp;s=89a0">"#,
            &footer(9196, "View 1312 comments"),
        ]
        .concat(),
    )
}

pub fn gallery_post_page() -> String {
    let slide = |id: &str| {
        format!(
            r#"<li><faceplate-img src="https://preview.redd.it/happy-halloween-to-the-hoa-president-v0-{id}.jpeg?width=640&amp;crop=smart&amp;auto=webp&amp;s=8dfb" alt="" role="presentation"></faceplate-img><img alt="r/fuckHOA - Happy Halloween to the HOA President" class=" h-full w-full object-contain relative" loading="lazy" src="https://preview.redd.it/happy-halloween-to-the-hoa-president-v0-{id}.jpeg?width=640&amp;crop=smart&amp;auto=webp&amp;s=8dfb"></li>"#
        )
    };
    post_page(
        "gallery",
        "https://www.reddit.com/gallery/1xm08ih",
        &[
            POST_HEADER,
            &title("Happy Halloween to the HOA President"),
            r#"<gallery-carousel post-id="t3_1xm08ih" advance-animation><ul>"#,
            &slide("zx0o44opyzyh1"),
            &slide("s913bdqpyzyh1"),
            "</ul></gallery-carousel>",
            &footer(44509, "View 3008 comments"),
        ]
        .concat(),
    )
}

pub fn video_post_page() -> String {
    let packaged = json!({ "playbackMp4s": { "duration": 22, "permutations": [
        { "source": { "bitrateBps": 158564, "dimensions": { "height": 220, "width": 392 }, "url": "https://packaged-media.redd.it/wyq5lk4ow7zh1/pb/m2-res_220p.mp4?m=DASHPlaylist.mpd&v=1&e=1793667600&s=5b64", "videoCodec": "H264" } },
        { "source": { "bitrateBps": 1930616, "dimensions": { "height": 1080, "width": 1920 }, "url": PACKAGED_1080, "videoCodec": "H264" } },
        { "source": { "bitrateBps": 680566, "dimensions": { "height": 480, "width": 854 }, "url": "https://packaged-media.redd.it/wyq5lk4ow7zh1/pb/m2-res_480p.mp4?m=DASHPlaylist.mpd&v=1&e=1793667600&s=ed13", "videoCodec": "H264" } },
    ] } });
    post_page(
        "video",
        "https://v.redd.it/wyq5lk4ow7zh1",
        &[
            POST_HEADER,
            &title("mrekk | Shogo Sakai - Luigi&#39;s Mansion Theme"),
            &format!(
                r#"<shreddit-player src="https://v.redd.it/wyq5lk4ow7zh1/HLSPlaylist.m3u8?f=hd%2CsubsAll%2ChlsSpecOrder&amp;v=1" class="block h-full w-full" autoplay packaged-media-json="{}"><source src="https://v.redd.it/wyq5lk4ow7zh1/HLSPlaylist.m3u8" type="application/vnd.apple.mpegURL"></shreddit-player>"#,
                escape_html(&packaged.to_string())
            ),
            &footer(233, "View 9 comments"),
        ]
        .concat(),
    )
}

pub fn text_post_page() -> String {
    post_page(
        "text",
        "https://www.reddit.com/r/askscience/comments/1xm08ih/is_there_a_theoretical_maximum/",
        &[
            POST_HEADER,
            r#"<a href="https://www.reddit.com/r/askscience/comments/1xm08ih/"><faceplate-tracker source="global" action="click" noun="embed"> <shreddit-embed-title>Is there a theoretical maximum size in humans (or any animal)?</shreddit-embed-title> </faceplate-tracker> </a> <read-more-button id="t3_1xm08ih" isforpost shouldforcerender></read-more-button> <div class="leading-[20px] relative"> <div id="t3_1xm08ih-post-rtjson-content" class="md mx-0 mb-xs text-neutral-content overflow-hidden h-[60px]" dir="auto"> <p> So people often joke about it
 but eventually it would cost too much blood right? </p><p>Is there some sort of hard limit?</p> </div> </div>"#,
            &footer(2712, "View 363 comments"),
        ]
        .concat(),
    )
}

pub fn unsupported_post_page() -> String {
    post_page(
        "link",
        "https://i.redd.it/dm85kmia6myh1.jpeg",
        r#"<div class="flex flex-col items-center"> <p class="text-14">Not supported post</p> </div>"#,
    )
}

fn comment_body(inner: &str) -> String {
    format!(
        r#"<div class="md text-14"><div id="t1_{COMMENT_ID}-post-rtjson-content" class="md" dir="auto">{inner}</div></div>"#
    )
}

pub fn text_comment_page() -> String {
    comment_page(
        None,
        &[
            COMMENT_HEADER,
            &comment_body(r#" <p> Furina in <a rpl href="/r/osugame/">r/osugame</a>? damn </p> "#),
            &footer(1, "View more on Reddit"),
        ]
        .concat(),
    )
}

pub fn image_comment_page() -> String {
    comment_page(
        Some("image"),
        &[
            COMMENT_HEADER,
            &comment_body(
                r#" <figure> <a href="https://preview.redd.it/pippi-x-furina-yuri-v0-p5v2kpn38myh1.png?width=581&amp;format=png&amp;auto=webp&amp;s=7759" rel="noopener ugc" target="_blank"> <img src="https://preview.redd.it/pippi-x-furina-yuri-v0-p5v2kpn38myh1.png?width=581&amp;format=png&amp;auto=webp&amp;s=7759" alt="Comment Image" width="240" height="auto" loading="lazy"> </a> </figure> "#,
            ),
            &footer(92, "View 3 comments"),
        ]
        .concat(),
    )
}

pub fn gif_comment_page() -> String {
    comment_page(
        Some("image"),
        &[
            COMMENT_HEADER,
            &comment_body(
                r#" <figure> <shreddit-async-loader bundleName="shreddit_aspect_ratio"> <shreddit-aspect-ratio id="nhvtlii5suxh1-aspect-ratio" is-embed style=" --aspect-ratio: 1;"> <shreddit-player src="https://preview.redd.it/nhvtlii5suxh1.gif?width=640&amp;format=mp4&amp;s=e38d" autoplay gif is-mobile> <source src="https://preview.redd.it/nhvtlii5suxh1.gif?width=640&amp;format=mp4&amp;s=e38d"> </shreddit-player> <template slot="error"> <div> <img alt="Media error" loading="eager" objectfit="contain" src=""> </div> </template> </shreddit-aspect-ratio> </shreddit-async-loader> </figure> "#,
            ),
            &footer(1372, "View 1 comment"),
        ]
        .concat(),
    )
}

pub fn video_comment_page() -> String {
    comment_page(
        Some("video"),
        &[
            COMMENT_HEADER,
            &comment_body(
                r#" <p> The long awaited sequel to </p> <figure> <div class="relative"> <div id="1wnr3rk6bmyh1-aspect-ratio" data-aspect-ratio-container> <div> <div> <img alt="Media error" loading="eager" objectfit="contain" src=""> <div> <p dir="auto"> Sorry, something went wrong when loading this video. </p> <a rpl name="app-link" href="https://reddit.onelink.me/MRHZ"> <span> View in app </span> </a> </div> </div> </div> </div> </div> </figure> "#,
            ),
            &footer(62, "View 2 comments"),
        ]
        .concat(),
    )
}

pub const DASH_PLAYLIST: &str = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT6.528S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011" type="static">
  <Period duration="PT6.528S" id="0">
    <AdaptationSet contentType="video" id="0" maxHeight="720" maxWidth="1280">
      <Representation bandwidth="233475" codecs="avc1.4d401e" height="220" id="0" mimeType="video/mp4" width="392">
        <BaseURL>CMAF_220.mp4</BaseURL>
      </Representation>
      <Representation bandwidth="1389792" codecs="avc1.4d401f" height="720" id="4" mimeType="video/mp4" width="1280">
        <BaseURL>CMAF_720.mp4</BaseURL>
      </Representation>
      <Representation bandwidth="930639" codecs="avc1.4d401f" height="480" id="3" mimeType="video/mp4" width="854">
        <BaseURL>CMAF_480.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="audio" id="1">
      <Representation audioSamplingRate="48000" bandwidth="68254" codecs="mp4a.40.2" id="5" mimeType="audio/mp4">
        <BaseURL>CMAF_AUDIO_64.mp4</BaseURL>
      </Representation>
      <Representation audioSamplingRate="48000" bandwidth="134268" codecs="mp4a.40.2" id="6" mimeType="audio/mp4">
        <BaseURL>CMAF_AUDIO_128.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
