// posthog doesnt actualyl add the tiles to the dashboard :geh:

import { loadDefinitions } from "../node_modules/@posthog/definitions/dist/apply/load.js";
import { loadConfig } from "../node_modules/@posthog/definitions/dist/client/config.js";
import { listManagedDashboards } from "../node_modules/@posthog/definitions/dist/resources/dashboard/client.js";
import { listManagedInsights } from "../node_modules/@posthog/definitions/dist/resources/insight/client.js";

const config = loadConfig({
    host: process.env.POSTHOG_HOST ?? "https://eu.posthog.com",
});

const request = async (method, path, body) => {
    const url = `${config.host}/api/projects/${config.projectId}/dashboards/${path}`;
    const response = await fetch(url, {
        method,
        headers: {
            authorization: `Bearer ${config.apiKey}`,
            "content-type": "application/json",
        },
        body: JSON.stringify(body),
    });
    if (!response.ok) {
        throw new Error(
            `${method} ${url} → ${response.status}\n${await response.text()}`,
        );
    }
    return response.json();
};

const idsByKey = (rows, prefix) =>
    new Map(
        rows.map((row) => [
            row.tags.find((tag) => tag.startsWith(prefix)).slice(prefix.length),
            row.id,
        ]),
    );

const loaded = await loadDefinitions(import.meta.dirname);
if (!loaded.ok) {
    throw new Error(JSON.stringify(loaded.error));
}

const dashboardIds = idsByKey(
    await listManagedDashboards(config),
    "iac:dashboards:",
);
const insightIds = idsByKey(await listManagedInsights(config), "iac:insights:");

for (const { spec } of loaded.value.get("dashboards")) {
    const id = dashboardIds.get(spec.key);
    const tiles = [];
    for (const { insight, layout } of spec.tiles) {
        const tile = await request("POST", `${id}/add_insight/`, {
            insight_id: insightIds.get(insight.key),
        });
        tiles.push({ id: tile.id, layouts: { sm: layout } });
    }
    await request("PATCH", `${id}/`, { tiles });
    console.log(`${spec.key}: ${tiles.length} tiles`);
}
