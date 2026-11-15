import { dashboard, insight, trends } from "@posthog/definitions";
import { by, dateRange, grid, is } from "../helpers";

const EMBED_REQUESTED = "embed requested";
const LINK_CLICKED = "link clicked";

const failed = is("outcome", "error");
const fresh = is("cache", "miss");

const freshVsCached = insight({
    key: "abembed-fresh-vs-cached",
    name: "Embeds: fresh vs cached",
    description: "Embed, api and component requests by cache hit or miss.",
    query: trends({
        series: [{ event: EMBED_REQUESTED }],
        breakdownFilter: by("cache"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const byPlatform = insight({
    key: "abembed-by-platform",
    name: "Embeds by platform",
    query: trends({
        series: [{ event: EMBED_REQUESTED }],
        breakdownFilter: by("platform"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const linkClicks = insight({
    key: "abembed-link-clicks",
    name: "Link clicks",
    description: "Browsers redirected to the original post.",
    query: trends({
        series: [{ event: LINK_CLICKED }],
        breakdownFilter: by("platform"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const outcomes = insight({
    key: "abembed-outcomes",
    name: "Embed outcomes",
    description: "ok, gated (login walled) or error.",
    query: trends({
        series: [{ event: EMBED_REQUESTED }],
        breakdownFilter: by("outcome"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const failureRate = insight({
    key: "abembed-failure-rate",
    name: "Failure rate by platform",
    query: trends({
        series: [
            { event: EMBED_REQUESTED, properties: [failed] },
            { event: EMBED_REQUESTED },
        ],
        breakdownFilter: by("platform"),
        trendsFilter: {
            formulaNodes: [
                { formula: "A / B * 100", custom_name: "Failure rate" },
            ],
            aggregationAxisFormat: "percentage",
        },
        dateRange,
    }),
});

const failuresByStatus = insight({
    key: "abembed-failures-by-status",
    name: "Failures by status",
    query: trends({
        series: [{ event: EMBED_REQUESTED, properties: [failed] }],
        breakdownFilter: by("status"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const fetchTime = insight({
    key: "abembed-fetch-time",
    name: "Fresh fetch time (p90)",
    description: "How long cache misses take to load the post.",
    query: trends({
        series: [
            {
                event: EMBED_REQUESTED,
                math: "p90",
                math_property: "duration_ms",
                properties: [fresh],
            },
        ],
        breakdownFilter: by("platform"),
        trendsFilter: { aggregationAxisFormat: "duration_ms" },
        dateRange,
    }),
});

const providerFailures = insight({
    key: "abembed-provider-failures",
    name: "Provider failures",
    description: "Failed provider calls made while fetching posts.",
    query: trends({
        series: [
            {
                event: EMBED_REQUESTED,
                math: "sum",
                math_property: "provider_failures",
                properties: [fresh],
            },
        ],
        breakdownFilter: by("platform"),
        dateRange,
    }),
});

const accountFallback = insight({
    key: "abembed-account-fallback",
    name: "Posts that needed an account",
    description: "Fresh fetches the anonymous request could not resolve.",
    query: trends({
        series: [
            {
                event: EMBED_REQUESTED,
                properties: [is("resolved_via", "account")],
            },
        ],
        breakdownFilter: by("platform"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const failuresByLinkKind = insight({
    key: "abembed-failures-by-link-kind",
    name: "Failures: share vs canonical links",
    query: trends({
        series: [{ event: EMBED_REQUESTED, properties: [failed] }],
        breakdownFilter: by("link_kind"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const consumers = insight({
    key: "abembed-consumers",
    name: "Who is unfurling",
    query: trends({
        series: [{ event: EMBED_REQUESTED }],
        breakdownFilter: by("consumer"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const endpoints = insight({
    key: "abembed-endpoints",
    name: "Embed, api and component requests",
    query: trends({
        series: [{ event: EMBED_REQUESTED }],
        breakdownFilter: by("endpoint"),
        trendsFilter: { display: "ActionsBar" },
        dateRange,
    }),
});

const rows = [
    [freshVsCached, byPlatform],
    [linkClicks, outcomes],
    [failureRate, failuresByStatus],
    [fetchTime, providerFailures],
    [accountFallback, failuresByLinkKind],
    [consumers, endpoints],
];

export default dashboard({
    key: "abembed",
    name: "abembed",
    description: "Embeds, failures and link clicks across every platform.",
    tiles: grid(rows),
});
