import { Resvg } from "@resvg/resvg-js";
import {
    type AlbumTrackRow,
    OpenGraph,
    OpenGraphAlbum,
    OpenGraphPlaylist,
} from "@spqtify/embed-image";
import { Effect, Schema } from "effect";
import { Vibrant } from "node-vibrant/node";
import satori from "satori";
import { fonts } from "./fonts";
import { buildAlbumTrackRows, clampTrackIndex } from "./track-rows";

export { clampTrackIndex } from "./track-rows";

export class GetAlbumArtError extends Schema.TaggedError<GetAlbumArtError>()(
    "GetAlbumArtError",
    {},
) {}

// TODO: return placeholder image on errors instead?
// stops request from failing if spotify cdn is down
export const getArtworkArrayBuffer = (albumArtUrl: string) =>
    Effect.gen(function* () {
        const imageArrayBuffer = yield* Effect.tryPromise({
            try: () => fetch(albumArtUrl).then((res) => res.arrayBuffer()),
            // TODO: how can i not eat this error
            catch: () => new GetAlbumArtError(),
        });

        return imageArrayBuffer;
    });

export class GetImagePaletteError extends Schema.TaggedError<GetImagePaletteError>()(
    "GetImagePaletteError",
    {},
) {}

export const getPaletteFromImage = (imageArrayBuffer: ArrayBuffer) =>
    Effect.gen(function* () {
        const palette = yield* Effect.tryPromise({
            try: () => Vibrant.from(Buffer.from(imageArrayBuffer)).getPalette(),
            // TODO: how can i not eat this error
            catch: () => new GetImagePaletteError(),
        });

        return {
            // fixes type issues
            Vibrant: palette.Vibrant ? { hex: palette.Vibrant.hex } : undefined,
            DarkVibrant: palette.DarkVibrant
                ? { hex: palette.DarkVibrant.hex }
                : undefined,
        };
    });

export class GenerateSvgDataError extends Schema.TaggedError<GenerateSvgDataError>()(
    "GenerateSvgDataError",
    {},
) {}

export class GeneratePngError extends Schema.TaggedError<GeneratePngError>()(
    "GeneratePngError",
    {},
) {}

const ALBUM_IMAGE_WIDTH = 800;
const ALBUM_IMAGE_MIN_HEIGHT = 250;
const ALBUM_IMAGE_MAX_HEIGHT = 600;
const ALBUM_BASE_HEIGHT = 210;
const ALBUM_ROW_HEIGHT = 26;

export const generateImage = (opts: {
    albumArt: ArrayBuffer;
    baseColor: string;
    gradientColor: string;
    songName: string;
    artist: string;
}) =>
    Effect.gen(function* () {
        const svgComp = OpenGraph(opts);

        const svgData = yield* Effect.tryPromise({
            try: () =>
                satori(svgComp, {
                    width: ALBUM_IMAGE_WIDTH,
                    height: 300,
                    fonts,
                }),
            catch: () => new GenerateSvgDataError(),
        });

        const pngBuffer = yield* Effect.try({
            try: () => {
                const resvg = new Resvg(svgData, {});
                return resvg.render().asPng();
            },
            catch: () => new GeneratePngError(),
        });

        return pngBuffer;
    });

export const calculateAlbumImageHeight = (
    visibleRows: number,
    hasOverflowRow: boolean,
): number => {
    const lineCount = visibleRows + (hasOverflowRow ? 1 : 0);
    const calculatedHeight = ALBUM_BASE_HEIGHT + lineCount * ALBUM_ROW_HEIGHT;
    return Math.max(
        ALBUM_IMAGE_MIN_HEIGHT,
        Math.min(calculatedHeight, ALBUM_IMAGE_MAX_HEIGHT),
    );
};

type CollectionImageOptions = {
    albumArt: ArrayBuffer;
    baseColor: string;
    gradientColor: string;
    tracks: readonly string[];
    currentTrackIndex: number;
    trackCountIsCapped?: boolean;
};

type CollectionImageRenderOptions = {
    albumArt: ArrayBuffer;
    baseColor: string;
    gradientColor: string;
    trackRows: AlbumTrackRow[];
    overflowCount: number;
    overflowCountIsMinimum: boolean;
    imageHeight: number;
};

const generateCollectionImage = (
    opts: CollectionImageOptions,
    render: (
        options: CollectionImageRenderOptions,
    ) => ReturnType<typeof OpenGraphAlbum>,
) =>
    Effect.gen(function* () {
        const normalizedTracks = opts.tracks
            .map((track) => track.trim())
            .filter((track) => track.length > 0);

        const clampedTrackIndex = clampTrackIndex(
            opts.currentTrackIndex,
            normalizedTracks.length,
        );
        const { trackRows, overflowCount, overflowCountIsMinimum } = buildAlbumTrackRows(
            normalizedTracks,
            clampedTrackIndex,
            opts.trackCountIsCapped,
        );
        const imageHeight = calculateAlbumImageHeight(
            trackRows.length,
            overflowCount > 0,
        );

        const svgComp = render({
            albumArt: opts.albumArt,
            baseColor: opts.baseColor,
            gradientColor: opts.gradientColor,
            trackRows,
            overflowCount,
            overflowCountIsMinimum,
            imageHeight,
        });

        const svgData = yield* Effect.tryPromise({
            try: () =>
                satori(svgComp, {
                    width: ALBUM_IMAGE_WIDTH,
                    height: imageHeight,
                    fonts,
                }),
            catch: () => new GenerateSvgDataError(),
        });

        const pngBuffer = yield* Effect.try({
            try: () => {
                const resvg = new Resvg(svgData, {
                    fitTo: {
                        mode: "width",
                        value: ALBUM_IMAGE_WIDTH,
                    },
                });
                return resvg.render().asPng();
            },
            catch: () => new GeneratePngError(),
        });

        return pngBuffer;
    });

export const generateAlbumImage = (
    opts: CollectionImageOptions & {
        titleText: string;
        artistText: string;
    },
) =>
    generateCollectionImage(opts, (imageOptions) =>
        OpenGraphAlbum({
            ...imageOptions,
            titleText: opts.titleText,
            artistText: opts.artistText,
        }),
    );

export const generatePlaylistImage = (
    opts: CollectionImageOptions & {
    playlistName: string;
    creatorName: string;
    },
) =>
    generateCollectionImage(opts, (imageOptions) =>
        OpenGraphPlaylist({
            ...imageOptions,
            playlistName: opts.playlistName,
            creatorName: opts.creatorName,
        }),
    );
