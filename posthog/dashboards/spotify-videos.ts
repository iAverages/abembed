import { dashboard, insight, trends } from "@posthog/definitions";
import { by, dateRange, grid, is } from "../helpers";

// apps/api/src/analytics.rs
const EMBED_REQUESTED = "embed requested";
const VIDEO_RENDERED = "video rendered";
const VIDEO_REQUESTED = "video requested";
const CACHE_UPDATED = "video cache updated";
const CACHE_EVICTED = "video cache evicted";
const CACHE_WRITE_SKIPPED = "video cache write skipped";

const ok = is("outcome", "ok");
const failed = is("outcome", "error");

const generated = insight({
    key: "spotify-videos-generated",
    name: "Videos generated",
    query: trends({
        series: [{ event: VIDEO_RENDERED, properties: [ok] }],
        breakdownFilter: by("media_type"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const renderFailureRate = insight({
    key: "spotify-videos-render-failure-rate",
    name: "Render failure rate",
    query: trends({
        series: [
            { event: VIDEO_RENDERED, properties: [failed] },
            { event: VIDEO_RENDERED },
        ],
        trendsFilter: {
            formulaNodes: [
                { formula: "A / B * 100", custom_name: "Failure rate" },
            ],
            aggregationAxisFormat: "percentage",
        },
        dateRange,
    }),
});

const renderTime = insight({
    key: "spotify-videos-render-time",
    name: "Render time (median and p90)",
    query: trends({
        series: ["median", "p90"].map((math) => ({
            event: VIDEO_RENDERED,
            math,
            math_property: "duration_ms",
            properties: [ok],
        })),
        trendsFilter: { aggregationAxisFormat: "duration_ms" },
        dateRange,
    }),
});

const videoSize = insight({
    key: "spotify-videos-size",
    name: "Video size in bytes (p90)",
    query: trends({
        series: [
            {
                event: VIDEO_RENDERED,
                math: "p90",
                math_property: "size_bytes",
            },
        ],
        breakdownFilter: by("media_type"),
        dateRange,
    }),
});

const bytesGenerated = insight({
    key: "spotify-videos-bytes-generated",
    name: "Bytes generated",
    query: trends({
        series: [
            {
                event: VIDEO_RENDERED,
                math: "sum",
                math_property: "size_bytes",
            },
        ],
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const embedVideoSource = insight({
    key: "spotify-videos-embed-source",
    name: "Where embed videos came from",
    description:
        "rendered, memory, storage (B2) or native (Spotify's own music video).",
    query: trends({
        series: [
            {
                event: EMBED_REQUESTED,
                properties: [is("platform", "spotify"), ok],
            },
        ],
        breakdownFilter: by("video_source"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const fileRequests = insight({
    key: "spotify-videos-file-requests",
    name: "Video file requests",
    description: "Video files served, by where the video came from.",
    query: trends({
        series: [{ event: VIDEO_REQUESTED, properties: [ok] }],
        breakdownFilter: by("video_source"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const fileFailures = insight({
    key: "spotify-videos-file-failures",
    name: "Failed video file requests",
    query: trends({
        series: [{ event: VIDEO_REQUESTED, properties: [failed] }],
        breakdownFilter: by("status"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const cacheEvictions = insight({
    key: "spotify-videos-cache-evictions",
    name: "Memory cache evictions",
    query: trends({
        series: [
            {
                event: CACHE_EVICTED,
                math: "sum",
                math_property: "eviction_count",
            },
        ],
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const cacheFill = insight({
    key: "spotify-videos-cache-fill",
    name: "Memory cache size in bytes (daily max)",
    query: trends({
        series: ["current_size_bytes", "max_size_bytes"].map(
            (math_property) => ({
                event: CACHE_UPDATED,
                math: "max",
                math_property,
            }),
        ),
        dateRange,
    }),
});

const cacheWritesSkipped = insight({
    key: "spotify-videos-cache-writes-skipped",
    name: "Skipped cache writes",
    query: trends({
        series: [{ event: CACHE_WRITE_SKIPPED }],
        breakdownFilter: by("reason"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const rows = [
    [generated, renderFailureRate],
    [renderTime, videoSize],
    [embedVideoSource, fileRequests],
    [fileFailures, bytesGenerated],
    [cacheEvictions, cacheFill],
    [cacheWritesSkipped],
];

export default dashboard({
    key: "spotify-videos",
    name: "spotify videos",
    description: "Video renders, video file requests and the memory cache.",
    tiles: grid(rows),
});
