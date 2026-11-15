export const dateRange = { date_from: "-30d" };

export const is = (key: string, value: string) => ({
    key,
    value: [value],
    operator: "exact",
    type: "event",
});

export const by = (breakdown: string) => ({
    breakdown,
    breakdown_type: "event",
});

export const grid = <T>(rows: T[][]) =>
    rows.flatMap((row, y) =>
        row.map((insight, x) => ({
            insight,
            layout: { x: x * 6, y: y * 5, w: 6, h: 5 },
        })),
    );
