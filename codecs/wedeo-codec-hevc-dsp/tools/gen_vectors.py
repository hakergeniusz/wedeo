#!/usr/bin/env python3
"""Generate FFmpeg-derived reference vectors for wedeo-codec-hevc-dsp.

The C functions are *extracted verbatim* from the FFmpeg source tree at
``~/Projects/FFmpeg`` and compiled into a small harness with stubbed FFmpeg
helpers. The harness feeds deterministic pseudo-random inputs through those
functions and prints Rust constants into ``src/reference_vectors.rs``.

Usage:
    python3 tools/gen_vectors.py
"""

from __future__ import annotations

import re
import subprocess
import tempfile
from pathlib import Path

FFMPEG = Path.home() / "Projects" / "FFmpeg" / "libavcodec"
CRATE = Path(__file__).resolve().parent.parent
OUT = CRATE / "src" / "reference_vectors.rs"


def read(rel: str) -> str:
    return (FFMPEG / rel).read_text(encoding="utf-8")


PRED = read("hevc/pred_template.c")
INTER = read("h26x/h2656_inter_template.c")
SAO = read("h26x/h2656_sao_template.c")
DEBLOCK = read("h26x/h2656_deblock_template.c")
DSP_T = read("hevc/dsp_template.c")
DSP_C = read("hevc/dsp.c")


def function(src: str, name: str) -> str:
    """Extract a complete C function whose signature contains FUNC(name)."""
    m = re.search(r"FUNC\(" + re.escape(name) + r"\)\s*\(", src)
    if not m:
        raise RuntimeError(f"function {name} not found")
    open_brace = src.index("{", m.end())
    depth = 0
    end = open_brace
    for i in range(open_brace, len(src)):
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
            if depth == 0:
                end = i + 1
                break
    start = src.rfind("\n", 0, m.start()) + 1
    return src[start:end] + "\n"


def region(src: str, start_marker: str, end_marker: str) -> str:
    a = src.index(start_marker)
    b = src.index(end_marker, a)
    return src[a:b]


def table(src: str, name: str) -> str:
    marker = name + ")[8][4] = {" if "epel" in name else name + ")[4][16] = {"
    pos = src.index(marker)
    start = src.index("{", pos)
    end = src.index("\n};", start) + 3
    body = src[start:end]
    return "static const int8_t " + name + "[][16] = " + body + "\n"


def transform_table(src: str) -> str:
    m = re.search(r"static const int8_t transform\[32\]\[32\] = \{(.*?)\n\};", src, re.S)
    return "static const int8_t transform[32][32] = {" + m.group(1) + "\n};\n"


PRELUDE = r"""
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define FFMIN(a, b) ((a) > (b) ? (b) : (a))
#define FFMAX(a, b) ((a) > (b) ? (a) : (b))
#define CMP(a, b) (((a) > (b)) - ((a) < (b)))

typedef uint8_t pixel;
typedef uint32_t pixel4;
#define av_always_inline inline
#define av_clip(a, lo, hi) ((a) < (lo) ? (lo) : (a) > (hi) ? (hi) : (a))
#define av_clip_pixel(a) av_clip((a), 0, (1 << BIT_DEPTH) - 1)
#define av_clip_int16(a) av_clip((a), -32768, 32767)
#define PIXEL_SPLAT_X4(v) ((pixel4)(uint32_t)(v) * 0x01010101u)
#define AV_WN4P(p, v) do { memcpy((p), &(v), 4); } while (0)
#define AV_RN4P(p) (*(const pixel4 *)(p))
#define MAX_PB_SIZE 64
#define MAX_TB_SIZE 32
#define AV_INPUT_BUFFER_PADDING_SIZE 64
#define QPEL_EXTRA_BEFORE 3
#define QPEL_EXTRA 7
#define EPEL_EXTRA_BEFORE 1
#define EPEL_EXTRA 3
#define POS(x, y) src[(x) + stride * (y)]
#define FUNC(a) a ## _8
"""

SOURCES = "\n".join(
    [
        # FFmpeg tables and macros, verbatim.
        transform_table(DSP_C),
        table(DSP_C, "ff_hevc_epel_filters"),
        table(DSP_C, "ff_hevc_qpel_filters"),
        function(DSP_T, "dequant"),
        function(DSP_T, "transform_rdpcm"),
        region(DSP_T, "#define SET(dst, x)", "#undef SET\n#undef SCALE"),
        "#undef SET\n#undef SCALE\n",
        region(INTER, "#define CHROMA_EXTRA_BEFORE", "static void FUNC(put_pixels)"),
        region(INTER, "#define LUMA_FILTER(src, stride)", "static void FUNC(put_luma_h)"),
        region(INTER, "#define CHROMA_FILTER(src, stride)", "static void FUNC(put_chroma_h)"),
        region(DSP_T, "#define QPEL_FILTER(src, stride)", "static void FUNC(put_hevc_qpel_bi_h)"),
        region(DSP_T, "#define EPEL_FILTER(src, stride)", "static void FUNC(put_hevc_epel_bi_h)"),
        region(
            DSP_T,
            "// line zero",
            '#include "libavcodec/h26x/h2656_deblock_template.c"',
        ),
        "\n".join(
            [
                function(PRED, name)
                for name in (
                    "ref_filter_3tap",
                    "ref_filter_strong",
                    "pred_planar",
                    "pred_dc",
                    "pred_angular",
                )
            ]
        ),
        "\n".join(
            [
                function(INTER, name)
                for name in (
                    "put_pixels",
                    "put_luma_h",
                    "put_luma_v",
                    "put_luma_hv",
                    "put_chroma_h",
                    "put_chroma_v",
                    "put_chroma_hv",
                    "put_uni_pixels",
                    "put_uni_luma_h",
                    "put_uni_luma_v",
                    "put_uni_luma_hv",
                    "put_uni_chroma_h",
                    "put_uni_chroma_v",
                    "put_uni_chroma_hv",
                    "put_uni_w_pixels",
                    "put_uni_luma_w_h",
                    "put_uni_luma_w_v",
                    "put_uni_luma_w_hv",
                    "put_uni_chroma_w_h",
                    "put_uni_chroma_w_v",
                    "put_uni_chroma_w_hv",
                )
            ]
        ),
        "\n".join(
            [
                function(DSP_T, name)
                for name in (
                    "put_hevc_pel_bi_pixels",
                    "put_hevc_pel_bi_w_pixels",
                    "put_hevc_qpel_bi_h",
                    "put_hevc_qpel_bi_v",
                    "put_hevc_qpel_bi_hv",
                    "put_hevc_qpel_bi_w_h",
                    "put_hevc_qpel_bi_w_v",
                    "put_hevc_qpel_bi_w_hv",
                    "put_hevc_epel_bi_h",
                    "put_hevc_epel_bi_v",
                    "put_hevc_epel_bi_hv",
                    "put_hevc_epel_bi_w_h",
                    "put_hevc_epel_bi_w_v",
                    "put_hevc_epel_bi_w_hv",
                )
            ]
        ),
        "\n".join(
            [function(DEBLOCK, name) for name in
             ("loop_filter_luma_strong", "loop_filter_luma_weak", "loop_filter_chroma_weak")]
        ),
        "\n".join(
            [
                function(DSP_T, name)
                for name in (
                    "hevc_loop_filter_luma",
                    "hevc_loop_filter_chroma",
                    "hevc_h_loop_filter_luma",
                    "hevc_v_loop_filter_luma",
                    "hevc_h_loop_filter_chroma",
                    "hevc_v_loop_filter_chroma",
                )
            ]
        ),
        "\n".join([function(SAO, name) for name in ("sao_band_filter", "sao_edge_filter")]),
    ]
)


MAIN = r"""
static unsigned int rng_state = 12345;
static unsigned int rnd(unsigned int n) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 17;
    rng_state ^= rng_state << 5;
    return rng_state % n;
}

static void emit(const char *name, const int *v, int n) {
    printf("pub const %s: &[i32] = &[\n    ", name);
    for (int i = 0; i < n; i++) {
        printf("%d,", v[i]);
        if ((i + 1) % 16 == 0) printf("\n    ");
    }
    printf("\n];\n\n");
}

static void emit_pixels(const char *name, const pixel *v, int n) {
    static int tmp[4096];
    for (int i = 0; i < n; i++) tmp[i] = v[i];
    emit(name, tmp, n);
}

static void emit_i16(const char *name, const int16_t *v, int n) {
    static int tmp[4096];
    for (int i = 0; i < n; i++) tmp[i] = v[i];
    emit(name, tmp, n);
}

#define NAMED(name, fn) do { fn; emit(name, buf, n); } while (0)

int main(void) {
    int n;
    pixel ref[4096], dst[4096], src2p[4096];
    int16_t tmp[4096], tmp2[4096], coeff[1024];
    int16_t offsetv[5] = {0, -3, 5, -7, 9};

    /* --- Intra prediction ---------------------------------------------- */
    {
        pixel corner = 90;
        pixel left[130], top[130], fl[130], ft[130];
        for (int log2 = 2; log2 <= 5; log2++) {
            int size = 1 << log2;
            n = 2 * size;
            corner = (pixel)(70 + log2 * 7);
            left[0] = top[0] = corner; /* FFmpeg's [-1] sample */
            for (int i = 0; i < n; i++) {
                left[i + 1] = (pixel)(30 + (i * 17 + log2 * 11) % 170);
                top[i + 1] = (pixel)(25 + (i * 29 + log2 * 5) % 180);
            }
            {
                char name[64];
                snprintf(name, sizeof(name), "INTRA_REF_LEFT_%d", size);
                emit_pixels(name, left + 1, n);
                snprintf(name, sizeof(name), "INTRA_REF_TOP_%d", size);
                emit_pixels(name, top + 1, n);
                snprintf(name, sizeof(name), "INTRA_CORNER_%d", size);
                int corner_i = corner; emit(name, &corner_i, 1);
            }

            memset(dst, 0, sizeof(dst));
            pred_planar_8(dst, top + 1, left + 1, size, log2);
            {
                char name[64]; snprintf(name, sizeof(name), "INTRA_PLANAR_%d", size);
                emit_pixels(name, dst, size * size);
            }

            memset(dst, 0, sizeof(dst));
            pred_dc_8(dst, top + 1, left + 1, size, log2, 0);
            {
                char name[64]; snprintf(name, sizeof(name), "INTRA_DC_LUMA_%d", size);
                emit_pixels(name, dst, size * size);
            }
            memset(dst, 0, sizeof(dst));
            pred_dc_8(dst, top + 1, left + 1, size, log2, 1);
            {
                char name[64]; snprintf(name, sizeof(name), "INTRA_DC_CHROMA_%d", size);
                emit_pixels(name, dst, size * size);
            }

            for (int mode = 2; mode <= 34; mode++) {
                /* All 33 angular modes at 4x4; a representative subset above. */
                if (size > 4 && !(mode == 2 || mode == 3 || mode == 9 || mode == 10 ||
                                 mode == 11 || mode == 17 || mode == 18 || mode == 19 ||
                                 mode == 25 || mode == 26 || mode == 27 || mode == 33 ||
                                 mode == 34)) continue;
                if (size == 32 && !(mode == 2 || mode == 10 || mode == 18 || mode == 26 || mode == 34)) continue;
                memset(dst, 0, sizeof(dst));
                pred_angular_8(dst, top + 1, left + 1, size, 0, mode, size);
                char name[64];
                snprintf(name, sizeof(name), "INTRA_ANG_%d_%d_L", size, mode);
                emit_pixels(name, dst, size * size);
                memset(dst, 0, sizeof(dst));
                pred_angular_8(dst, top + 1, left + 1, size, 1, mode, size);
                snprintf(name, sizeof(name), "INTRA_ANG_%d_%d_C", size, mode);
                emit_pixels(name, dst, size * size);
            }
        }
        /* Reference filters (arrays carry a [-1] corner sample, as in FFmpeg). */
        for (int log2 = 3; log2 <= 5; log2++) {
            int size = 1 << log2, n2 = 2 * size;
            pixel la[130], ta[130], fla[130], fta[130];
            corner = 100;
            la[0] = ta[0] = corner;
            for (int i = 0; i < n2; i++) {
                la[i + 1] = (pixel)(20 + (i * 23) % 190);
                ta[i + 1] = (pixel)(35 + (i * 37) % 180);
            }
            {
                char name[64];
                snprintf(name, sizeof(name), "REF_FILT3_INPUT_LEFT_%d", size);
                emit_pixels(name, la + 1, n2);
                snprintf(name, sizeof(name), "REF_FILT3_INPUT_TOP_%d", size);
                emit_pixels(name, ta + 1, n2);
            }
            ref_filter_3tap_8(fla + 1, fta + 1, la + 1, ta + 1, size);
            char name[64];
            snprintf(name, sizeof(name), "REF_FILT3_LEFT_%d", size);
            emit_pixels(name, fla + 1, n2);
            snprintf(name, sizeof(name), "REF_FILT3_TOP_%d", size);
            emit_pixels(name, fta + 1, n2);
        }
        {
            pixel la[130], ta[130], fta[130];
            corner = 80;
            la[0] = ta[0] = corner;
            for (int i = 0; i < 64; i++) {
                la[i + 1] = (pixel)(20 + i * 3);
                ta[i + 1] = (pixel)(30 + i * 2);
            }
            la[64] = 180; ta[64] = 170;
            emit_pixels("REF_STRONG_INPUT_LEFT", la + 1, 64);
            emit_pixels("REF_STRONG_INPUT_TOP", ta + 1, 64);
            ref_filter_strong_8(fta + 1, la + 1, ta + 1);
            emit_pixels("REF_STRONG_TOP", fta + 1, 64);
            emit_pixels("REF_STRONG_LEFT", la + 1, 64);
        }
    }

    /* --- Inter prediction ---------------------------------------------- */
    {
        const int w = 8, h = 8, stride = 16, x = 4, y = 4;
        for (int i = 0; i < (h + 8) * stride; i++) ref[i] = (pixel)rnd(256);
        for (int i = 0; i < h * MAX_PB_SIZE; i++) src2p[i] = (pixel)rnd(256);
        for (int i = 0; i < h * MAX_PB_SIZE; i++) tmp2[i] = (int16_t)(((int)src2p[i] << 6) - 2000);
        emit_pixels("INTER_SRC", ref, (h + 8) * stride);
        emit_i16("INTER_SRC2", tmp2, h * MAX_PB_SIZE);

        struct { int mx, my; } lcases[] = {{0,0},{1,0},{0,2},{3,3},{2,1}};
        for (unsigned k = 0; k < sizeof(lcases)/sizeof(lcases[0]); k++) {
            int mx = lcases[k].mx, my = lcases[k].my;
            char name[64];
            memset(tmp, 0, sizeof(tmp));
            if (mx == 0 && my == 0) {
                put_pixels_8(tmp, ref + y * stride + x, stride, h, NULL, NULL, w);
            } else if (my == 0) {
                put_luma_h_8(tmp, ref + y * stride + x, stride, h,
                             (int8_t *)ff_hevc_qpel_filters[mx], NULL, w);
            } else if (mx == 0) {
                put_luma_v_8(tmp, ref + y * stride + x, stride, h,
                             NULL, (int8_t *)ff_hevc_qpel_filters[my], w);
            } else {
                put_luma_hv_8(tmp, ref + y * stride + x, stride, h,
                              (int8_t *)ff_hevc_qpel_filters[mx], (int8_t *)ff_hevc_qpel_filters[my], w);
           }
            snprintf(name, sizeof(name), "INTER_PUT_LUMA_%d_%d", mx, my);
            emit_i16(name, tmp, h * MAX_PB_SIZE);
            memset(dst, 0, sizeof(dst));
            if (mx == 0 && my == 0) {
                put_uni_pixels_8(dst, w, ref + y * stride + x, stride, h, NULL, NULL, w);
            } else if (my == 0) {
                put_uni_luma_h_8(dst, w, ref + y * stride + x, stride, h,
                                 (int8_t *)ff_hevc_qpel_filters[mx], NULL, w);
            } else if (mx == 0) {
                put_uni_luma_v_8(dst, w, ref + y * stride + x, stride, h,
                                 NULL, (int8_t *)ff_hevc_qpel_filters[my], w);
            } else {
                put_uni_luma_hv_8(dst, w, ref + y * stride + x, stride, h,
                                  (int8_t *)ff_hevc_qpel_filters[mx], (int8_t *)ff_hevc_qpel_filters[my], w);
            }
            snprintf(name, sizeof(name), "INTER_UNI_LUMA_%d_%d", mx, my);
            emit_pixels(name, dst, h * w);
            memset(dst, 0, sizeof(dst));
            if (mx == 0 && my == 0) {
                put_hevc_pel_bi_pixels_8(dst, w, ref + y * stride + x, stride, tmp2, h, 0, 0, w);
            } else if (my == 0) {
                put_hevc_qpel_bi_h_8(dst, w, ref + y * stride + x, stride, tmp2, h, mx, 0, w);
            } else if (mx == 0) {
                put_hevc_qpel_bi_v_8(dst, w, ref + y * stride + x, stride, tmp2, h, 0, my, w);
            } else {
                put_hevc_qpel_bi_hv_8(dst, w, ref + y * stride + x, stride, tmp2, h, mx, my, w);
            }
            snprintf(name, sizeof(name), "INTER_BI_LUMA_%d_%d", mx, my);
            emit_pixels(name, dst, h * w);
        }
        struct { int mx, my; } ccases[] = {{0,0},{1,0},{0,3},{5,7}};
        for (unsigned k = 0; k < sizeof(ccases)/sizeof(ccases[0]); k++) {
            int mx = ccases[k].mx, my = ccases[k].my;
            char name[64];
            memset(tmp, 0, sizeof(tmp));
            if (mx == 0 && my == 0) {
                put_pixels_8(tmp, ref + y * stride + x, stride, h, NULL, NULL, w);
            } else {
                if (my == 0) {
                    put_chroma_h_8(tmp, ref + y * stride + x, stride, h,
                                   (int8_t *)ff_hevc_epel_filters[mx], NULL, w);
                } else if (mx == 0) {
                    put_chroma_v_8(tmp, ref + y * stride + x, stride, h,
                                   NULL, (int8_t *)ff_hevc_epel_filters[my], w);
                } else {
                    put_chroma_hv_8(tmp, ref + y * stride + x, stride, h,
                                    (int8_t *)ff_hevc_epel_filters[mx], (int8_t *)ff_hevc_epel_filters[my], w);
                }
            }
            snprintf(name, sizeof(name), "INTER_PUT_CHROMA_%d_%d", mx, my);
            emit_i16(name, tmp, h * MAX_PB_SIZE);
            memset(dst, 0, sizeof(dst));
            if (mx == 0 && my == 0) {
                put_uni_pixels_8(dst, w, ref + y * stride + x, stride, h, NULL, NULL, w);
            } else {
                if (my == 0) {
                    put_uni_chroma_h_8(dst, w, ref + y * stride + x, stride, h,
                                       (int8_t *)ff_hevc_epel_filters[mx], NULL, w);
                } else if (mx == 0) {
                    put_uni_chroma_v_8(dst, w, ref + y * stride + x, stride, h,
                                       NULL, (int8_t *)ff_hevc_epel_filters[my], w);
                } else {
                    put_uni_chroma_hv_8(dst, w, ref + y * stride + x, stride, h,
                                        (int8_t *)ff_hevc_epel_filters[mx], (int8_t *)ff_hevc_epel_filters[my], w);
                }
            }
            snprintf(name, sizeof(name), "INTER_UNI_CHROMA_%d_%d", mx, my);
            emit_pixels(name, dst, h * w);
            memset(dst, 0, sizeof(dst));
            if (mx == 0 && my == 0) {
                put_hevc_pel_bi_pixels_8(dst, w, ref + y * stride + x, stride, tmp2, h, 0, 0, w);
            } else {
                if (my == 0) {
                    put_hevc_epel_bi_h_8(dst, w, ref + y * stride + x, stride, tmp2, h, mx, 0, w);
                } else if (mx == 0) {
                    put_hevc_epel_bi_v_8(dst, w, ref + y * stride + x, stride, tmp2, h, 0, my, w);
                } else {
                    put_hevc_epel_bi_hv_8(dst, w, ref + y * stride + x, stride, tmp2, h, mx, my, w);
                }
            }
            snprintf(name, sizeof(name), "INTER_BI_CHROMA_%d_%d", mx, my);
            emit_pixels(name, dst, h * w);
        }
        /* Integer (mx=my=0) and weighted paths. */
        memset(tmp, 0, sizeof(tmp));
        put_pixels_8(tmp, ref + y * stride + x, stride, h, NULL, NULL, w);
        emit_i16("INTER_PUT_PEL", tmp, h * MAX_PB_SIZE);
        memset(dst, 0, sizeof(dst));
        put_uni_w_pixels_8(dst, w, ref + y * stride + x, stride, h, 5, 26, 3, NULL, NULL, w);
        emit_pixels("INTER_UNI_W_PEL", dst, h * w);
        memset(dst, 0, sizeof(dst));
        put_hevc_pel_bi_w_pixels_8(dst, w, ref + y * stride + x, stride, tmp2, h, 5, 13, 19, 4, 0, 0, w);
        emit_pixels("INTER_BI_W_PEL", dst, h * w);
        memset(dst, 0, sizeof(dst));
        put_uni_luma_w_hv_8(dst, w, ref + y * stride + x, stride, h, 5, 26, -2,
                            (int8_t *)ff_hevc_qpel_filters[1], (int8_t *)ff_hevc_qpel_filters[2], w);
        emit_pixels("INTER_UNI_W_LUMA", dst, h * w);
        memset(dst, 0, sizeof(dst));
        put_hevc_qpel_bi_w_hv_8(dst, w, ref + y * stride + x, stride, tmp2, h, 5, 13, 19, 4, 2, 1, w);
        emit_pixels("INTER_BI_W_LUMA", dst, h * w);
        memset(dst, 0, sizeof(dst));
        put_uni_chroma_w_hv_8(dst, w, ref + y * stride + x, stride, h, 4, 12, 1,
                              (int8_t *)ff_hevc_epel_filters[4], (int8_t *)ff_hevc_epel_filters[3], w);
        emit_pixels("INTER_UNI_W_CHROMA", dst, h * w);
        memset(dst, 0, sizeof(dst));
        put_hevc_epel_bi_w_hv_8(dst, w, ref + y * stride + x, stride, tmp2, h, 4, 9, 7, 2, 4, 5, w);
        emit_pixels("INTER_BI_W_CHROMA", dst, h * w);
    }

    /* --- Transforms ----------------------------------------------------- */
    {
        for (int log2 = 2; log2 <= 5; log2++) {
            int size = 1 << log2;
            n = size * size;
            for (int i = 0; i < n; i++) coeff[i] = (int16_t)((int)rnd(2000) - 1000);
            char name[64];
            {
                char name[64]; snprintf(name, sizeof(name), "COEFF_INPUT_%d", size);
                emit_i16(name, coeff, n);
            }
            int16_t saved[1024]; memcpy(saved, coeff, n * sizeof(int16_t));
            dequant_8(coeff, log2);
            snprintf(name, sizeof(name), "DEQUANT_8_%d", size);
            emit_i16(name, coeff, n);
            memcpy(coeff, saved, n * sizeof(int16_t));
            (void)transform[0][0];
            /* Direct calls to the generated-size IDCT functions. */
            if (log2 == 2) transform_4x4_luma_8(coeff);
            else if (log2 == 3) idct_8x8_8(coeff, 0);
            else if (log2 == 4) idct_16x16_8(coeff, 0);
            else idct_32x32_8(coeff, 0);
            snprintf(name, sizeof(name), "IDCT_%d", size);
            emit_i16(name, coeff, n);
            memcpy(coeff, saved, n * sizeof(int16_t));
            if (log2 == 3) idct_8x8_8(coeff, 4);
            else if (log2 == 4) idct_16x16_8(coeff, 8);
            else if (log2 == 5) idct_32x32_8(coeff, 12);
            if (log2 >= 3) {
                snprintf(name, sizeof(name), "IDCT_LIMIT_%d", size);
                emit_i16(name, coeff, n);
            }
            memcpy(coeff, saved, n * sizeof(int16_t));
            if (log2 >= 3) {
                if (log2 == 3) idct_8x8_dc_8(coeff);
                else if (log2 == 4) idct_16x16_dc_8(coeff);
                else idct_32x32_dc_8(coeff);
                snprintf(name, sizeof(name), "IDCT_DC_%d", size);
                emit_i16(name, coeff, n);
            }
            memcpy(coeff, saved, n * sizeof(int16_t));
            transform_rdpcm_8(coeff, log2, 0);
            snprintf(name, sizeof(name), "RDPCM_H_%d", size);
            emit_i16(name, coeff, n);
            memcpy(coeff, saved, n * sizeof(int16_t));
            transform_rdpcm_8(coeff, log2, 1);
            snprintf(name, sizeof(name), "RDPCM_V_%d", size);
            emit_i16(name, coeff, n);
        }
    }

    /* --- Deblocking ----------------------------------------------------- */
    {
        int stride = 16;
        pixel block[16 * 16];
        for (int i = 0; i < 16 * stride; i++) block[i] = (pixel)rnd(256);
        int index = 6 * stride + 6;
        emit_pixels("DEBLOCK_INPUT", block, 16 * stride);
        int32_t tc[2] = {10, 12};
        uint8_t nop[2] = {0, 0}, noq[2] = {0, 1};
        pixel before[16 * 16]; memcpy(before, block, sizeof(block));
        hevc_v_loop_filter_luma_8(block + index, stride, 26, tc, nop, noq);
        emit_pixels("DEBLOCK_LUMA", block, 16 * stride);
        memcpy(block, before, sizeof(block));
        loop_filter_luma_strong_8(block + index, 1, stride, 12, 24, 24, 0, 0);
        emit_pixels("DEBLOCK_LUMA_STRONG", block, 16 * stride);
        memcpy(block, before, sizeof(block));
        loop_filter_luma_weak_8(block + index, 1, stride, 10, 26, 0, 0, 2, 2);
        emit_pixels("DEBLOCK_LUMA_WEAK", block, 16 * stride);
        memcpy(block, before, sizeof(block));
        hevc_h_loop_filter_chroma_8(block + index, stride, tc, nop, noq);
        emit_pixels("DEBLOCK_CHROMA", block, 16 * stride);
    }

    /* --- SAO ------------------------------------------------------------ */
    {
        int w = 8, h = 8, stride = 10;
        const int sao_stride = 2 * MAX_PB_SIZE + AV_INPUT_BUFFER_PADDING_SIZE;
        static pixel sao_src[4096];
        for (int i = 0; i < (h + 2) * sao_stride; i++) sao_src[i] = (pixel)rnd(256);
        emit_pixels("SAO_SRC", sao_src, (h + 2) * sao_stride);
        memset(dst, 0, sizeof(dst));
        sao_band_filter_8(dst, sao_src + sao_stride + 1, w, sao_stride, offsetv, 7, w, h);
        emit_pixels("SAO_BAND", dst, h * w);
        for (int eo = 0; eo < 4; eo++) {
            memset(dst, 0, sizeof(dst));
            sao_edge_filter_8(dst, sao_src + sao_stride + 1, w, offsetv, eo, w, h);
            char name[64]; snprintf(name, sizeof(name), "SAO_EDGE_%d", eo);
            emit_pixels(name, dst, h * w);
        }
    }
    return 0;
}
"""


def build_harness(bit_depth: int) -> str:
    prelude = PRELUDE
    main = MAIN
    if bit_depth != 8:
        prelude = prelude.replace("#define FUNC(a) a ## _8", f"#define FUNC(a) a ## _{bit_depth}")
        main = re.sub(r"\b([A-Za-z_][A-Za-z0-9_]*)_8\b(?=\s*\()", lambda m: m.group(1) + f"_{bit_depth}", main)
    return prelude + f"#define BIT_DEPTH {bit_depth}\n" + SOURCES + main


def run(bit_depth: int) -> str:
    with tempfile.TemporaryDirectory(prefix="wedeo-hevc-dsp-vectors-") as tmp:
        src = Path(tmp) / "harness.c"
        exe = Path(tmp) / "harness"
        src.write_text(build_harness(bit_depth), encoding="utf-8")
        compile_cmd = [
            "cc", "-std=c11", "-O0", "-w", "-o", str(exe), str(src), "-lm"
        ]
        subprocess.run(compile_cmd, check=True)
        return subprocess.run([str(exe)], check=True, capture_output=True, text=True).stdout


def main() -> None:
    parts = [
        "// Generated by tools/gen_vectors.py from FFmpeg's C source.\n",
        "// Regenerate with: python3 codecs/wedeo-codec-hevc-dsp/tools/gen_vectors.py\n",
    ]
    parts.append(run(8))
    # A 10-bit harness provides extra dequant / SAO vectors with a _10 suffix.
    ten = run(10)
    ten = re.sub(r"pub const (\w+):", lambda m: "pub const " + m.group(1) + "_10:", ten)
    # Keep only the vectors whose 10-bit behaviour differs.
    wanted = ("DEQUANT_8_",)
    kept = []
    for block in ten.split("\n\n"):
        if any(name in block for name in wanted):
            kept.append(block)
    parts.append("\n\n".join(kept))
    body = "\n".join(parts)
    names = sorted(set(re.findall(r"pub const (\w+):", body)))
    lookup = ["pub fn get(name: &str) -> &'static [i32] {", "    match name {"]
    lookup += [f'        "{n}" => {n},' for n in names]
    lookup += ["        _ => panic!(\"unknown reference vector {name}\"),", "    }", "}", ""]
    OUT.write_text(body + "\n" + "\n".join(lookup), encoding="utf-8")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes, {len(names)} vectors)")


if __name__ == "__main__":
    main()
