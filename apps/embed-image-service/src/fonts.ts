/** https://github.com/juliusmarminge/jumr.dev/blob/main/app/og-image/get-fonts.ts */

export async function getFont<TWeights extends readonly number[]>({
    family,
    weights,
    text,
}: {
    family: string;
    weights: TWeights;
    text?: string;
}): Promise<Record<TWeights[number], ArrayBuffer>> {
    const API = `https://fonts.googleapis.com/css2?family=${family}:wght@${weights.join(
        ";",
    )}${text ? `&text=${encodeURIComponent(text)}` : ""}`;

    const css = await (
        await fetch(API, {
            headers: {
                // Make sure it returns TTF.
                "User-Agent":
                    "Mozilla/5.0 (Macintosh; U; Intel Mac OS X 10_6_8; de-at) AppleWebKit/533.21.1 (KHTML, like Gecko) Version/5.0.5 Safari/533.21.1",
            },
        })
    ).text();

    const fonts = css
        .split("@font-face {")
        .splice(1)
        .map((font) => {
            const u = /src: url\((.+)\) format\('(opentype|truetype)'\)/.exec(
                font,
            );
            const w = /font-weight: (\d+)/.exec(font);
            return u?.[1] && w?.[1]
                ? { url: u[1], weight: Number.parseInt(w[1]) }
                : null;
        })
        .filter(
            (font): font is { url: string; weight: TWeights[number] } => !!font,
        );

    const promises = fonts.map(async (font) => {
        const res = await fetch(font.url);
        return [font.weight, await res.arrayBuffer()];
    });

    // Object.fromEntries is typed as returning any *sigh*
    // eslint-disable-next-line @typescript-eslint/no-unsafe-return
    return Object.fromEntries(await Promise.all(promises));
}

const families = [
    "Inter",
    "Noto Sans JP",
    "Noto Sans KR",
    "Noto Sans SC",
    "Noto Sans TC",
    "Noto Sans HK",
    "Noto Sans Hebrew",
    "Noto Sans Thai",
    "Noto Sans Devanagari",
    "Noto Sans Bengali",
    "Noto Sans Tamil",
    "Noto Sans Telugu",
    "Noto Sans Gujarati",
    "Noto Sans Gurmukhi",
    "Noto Sans Kannada",
    "Noto Sans Malayalam",
    "Noto Sans Oriya",
    "Noto Sans Ethiopic",
];

export const fonts = (
    await Promise.all(
        families.map(async (name) => {
            const font = await getFont({
                family: name,
                weights: [400, 700] as const,
            });
            return [
                { name, data: font[400], weight: 400 as const },
                { name, data: font[700], weight: 700 as const },
            ];
        }),
    )
).flat();
