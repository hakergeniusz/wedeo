# FFmpeg H.264 PAFF (Picture Adaptive Frame/Field) Implementation Reference

Source: `/var/cache/zstd5/ffmpeg-src-w62/libavcodec/` (FFmpeg 8.0.git, commit `05c674c913`)

## Status summary (2026-11-27)

Wedeo now pairs top/bottom fields and uses field-view MC as a bounded step
(commit `e61b4af`, Appendix Q), with documented trade-offs. Full interlaced
pixel parity is still open. Appendices before Q record the earlier state and
the then-current FRext baseline of 42/69; current FRext parity is 44/69 and the
four-suite total is 199/283. See `STATUS.md` for the compact snapshot.

## CRITICAL CORRECTION TO THE INITIAL ASSUMPTION

FFmpeg does **NOT** decode each field as a separate half-height picture and then combine them. Instead:

- A **single full-height `AVFrame`** is allocated (the normal frame buffer at full coded height).
- Top field lines are written into **even rows**, bottom field lines into **odd rows** of that same buffer.
- The **second field reuses the same `H264Picture`** (`h->cur_pic_ptr` is NOT reallocated). `h264_frame_start()` is called only for a frame or the *first* field of a pair.
- Field macroblocks use **doubled strides** (`linesize*2`, `uvlinesize*2`) so each field line steps 2 frame lines.

This is the classic interlaced storage layout. The "combining" is implicit: both fields are already interleaved in one buffer.

---

## 0. Core data structures and constants

### `mpegutils.h:31-33`
```c
#define PICT_TOP_FIELD     1
#define PICT_BOTTOM_FIELD  2
#define PICT_FRAME         3
```
`reference` is a **bitmask**: bit0 = top field ref, bit1 = bottom field ref, `PICT_FRAME=3` = both.

### `h264dec.h:60-82` - macros
```c
#ifdef ALLOW_INTERLACE
#define MB_FIELD(sl)  (sl)->mb_field_decoding_flag
#define FRAME_MBAFF(h) (h)->mb_aff_frame
#define FIELD_PICTURE(h) ((h)->picture_structure != PICT_FRAME)
...
#endif
#define FIELD_OR_MBAFF_PICTURE(h) (FRAME_MBAFF(h) || FIELD_PICTURE(h))
```
Note `FIELD_PICTURE()` is used with both `H264Context*` and `H264SliceContext*` (both have `picture_structure`).

### `h264dec.h:125-165` - `H264Picture` (key fields)
```c
typedef struct H264Picture {
    AVFrame *f;
    ThreadFrame tf;
    ...
    int field_poc[2];       ///< top/bottom POC
    int poc;                ///< frame POC
    int frame_num;          ///< frame_num (raw frame_num from slice header)
    ...
    int mbaff;              ///< 1 -> MBAFF frame 0-> not MBAFF
    int field_picture;      ///< whether or not picture was encoded in separate fields
#define DELAYED_PIC_REF  (1 << 2)
    int reference;
    ...
} H264Picture;
```

### `h264dec.h:167-176` - `H264Ref` (reference list entry)
```c
typedef struct H264Ref {
    uint8_t *data[3];
    int linesize[3];
    int reference;
    int poc;
    int pic_id;
    const H264Picture *parent;
} H264Ref;
```

### `h264dec.h:250-253` - `H264SliceContext` interlace flags
```c
    int picture_structure;
    int mb_field_decoding_flag;
    int mb_mbaff;               ///< mb_aff_frame && mb_field_decoding_flag
```

### `h264dec.h:417-421` - `H264Context` interlace flags
```c
    // interlacing specific flags
    int mb_aff_frame;
    int picture_structure;
    int first_field;
```

### `h264dec.h:356-357, 473-477, 558`
```c
    H264Picture *cur_pic_ptr;
    H264Picture cur_pic;
    ...
    H264POCContext poc;
    H264Ref default_ref[2];
    H264Picture *short_ref[32];
    H264Picture *long_ref[32];
    ...
    int missing_fields;
```

---

## 1. Parsing `field_pic_flag` / `bottom_field_flag` and storing into H264Picture

### `h264_slice.c:1738-1818`, `h264_slice_header_parse()`

```c
    int field_pic_flag, bottom_field_flag;
    int picture_structure;
    ...
    if (sps->frame_mbs_only_flag) {
        picture_structure = PICT_FRAME;
    } else {
        if (!sps->direct_8x8_inference_flag && slice_type == AV_PICTURE_TYPE_B) { ... error ... }
        field_pic_flag = get_bits1(&sl->gb);
        if (field_pic_flag) {
            bottom_field_flag = get_bits1(&sl->gb);
            picture_structure = PICT_TOP_FIELD + bottom_field_flag;   // top=1, bottom=2
        } else {
            picture_structure = PICT_FRAME;
        }
    }
    sl->picture_structure      = picture_structure;
    sl->mb_field_decoding_flag = picture_structure != PICT_FRAME;

    if (picture_structure == PICT_FRAME) {
        sl->curr_pic_num = sl->frame_num;
        sl->max_pic_num  = 1 << sps->log2_max_frame_num;
    } else {
        sl->curr_pic_num = 2 * sl->frame_num + 1;
        sl->max_pic_num  = 1 << (sps->log2_max_frame_num + 1);
    }
```

Key points:
- `field_pic_flag` only present when `!sps->frame_mbs_only_flag`.
- `bottom_field_flag` only present when `field_pic_flag == 1`.
- `picture_structure = PICT_TOP_FIELD + bottom_field_flag` (1+0=top, 1+1=bottom).
- `mb_field_decoding_flag = 1` for any PAFF field (so all MBs are field-coded).
- Field pic_num space is doubled: `curr_pic_num = 2*frame_num+1`, `max_pic_num = 1<<(log2_max_frame_num+1)`.

### POC-related slice header fields (`h264_slice.c:1820-1850`)
```c
    sl->poc_lsb = 0;
    sl->delta_poc_bottom = 0;
    if (sps->poc_type == 0) {
        sl->poc_lsb = get_bits(&sl->gb, sps->log2_max_poc_lsb);
        if (pps->pic_order_present == 1 && picture_structure == PICT_FRAME)
            sl->delta_poc_bottom = get_se_golomb(&sl->gb);
    }
    sl->delta_poc[0] = sl->delta_poc[1] = 0;
    if (sps->poc_type == 1 && !sps->delta_pic_order_always_zero_flag) {
        sl->delta_poc[0] = get_se_golomb(&sl->gb);
        if (pps->pic_order_present == 1 && picture_structure == PICT_FRAME)
            sl->delta_poc[1] = get_se_golomb(&sl->gb);
    }
```
`delta_poc_bottom` / `delta_poc[1]` are **frame-only**; absent in field slice headers.

### Transfer to context - `h264_field_start()` (`h264_slice.c:1445-1453`)
```c
    last_pic_droppable   = h->droppable;
    last_pic_structure   = h->picture_structure;
    h->droppable         = (nal->ref_idc == 0);
    h->picture_structure = sl->picture_structure;

    h->poc.frame_num        = sl->frame_num;
    h->poc.poc_lsb          = sl->poc_lsb;
    h->poc.delta_poc_bottom = sl->delta_poc_bottom;
    h->poc.delta_poc[0]     = sl->delta_poc[0];
    h->poc.delta_poc[1]     = sl->delta_poc[1];
```

### Stored in the picture - `h264_frame_start()` (`h264_slice.c:486-600`)
```c
    pic->reference              = h->droppable ? 0 : h->picture_structure;
    pic->field_picture          = h->picture_structure != PICT_FRAME;
    pic->frame_num               = h->poc.frame_num;
    ...
    h->cur_pic_ptr = pic;
    ...
    h->cur_pic_ptr->reference = 0;
    h->cur_pic_ptr->field_poc[0] = h->cur_pic_ptr->field_poc[1] = INT_MAX;
    h->mb_aff_frame = h->ps.sps->mb_aff && (h->picture_structure == PICT_FRAME);
```
So `H264Picture.field_picture` is set from `picture_structure`, and `field_poc[]` is initialized to `INT_MAX` (the marker for "field not yet decoded"). The `reference` field becomes a **bitmask** of which fields are reference fields.

---

## 2. Top/bottom field pairing into one output frame

### `h264_field_start()` (`h264_slice.c:1421-1736`) - the heart of PAFF

This is called "right after decoding the slice header for a first slice in a field (or a frame)". It decides new frame vs second field.

**Phase A - detect complementary pair / finalize previous field (`h264_slice.c:1483-1532`):**
```c
    if (h->first_field) {
        int last_field = last_pic_structure == PICT_BOTTOM_FIELD;
        ...
        if (h->cur_pic_ptr->tf.owner[last_field] == h->avctx) {
            ff_thread_report_progress(&h->cur_pic_ptr->tf, INT_MAX, last_field);
        }

        if (!FIELD_PICTURE(h) || h->picture_structure == last_pic_structure) {
            /* Previous field is unmatched. Don't display it, but let it
             * remain for reference if marked as such. */
            if (last_pic_structure != PICT_FRAME) {
                ff_thread_report_progress(&h->cur_pic_ptr->tf, INT_MAX,
                                          last_pic_structure == PICT_TOP_FIELD);
            }
        } else {
            if (h->cur_pic_ptr->frame_num != h->poc.frame_num) {
                /* Different frame_nums: throw away previous field except for reference */
                ...
            } else {
                /* Second field in complementary pair */
                if (!((last_pic_structure   == PICT_TOP_FIELD &&
                       h->picture_structure == PICT_BOTTOM_FIELD) ||
                      (last_pic_structure   == PICT_BOTTOM_FIELD &&
                       h->picture_structure == PICT_TOP_FIELD))) {
                    av_log(..., "Invalid field mode combination %d/%d\n", ...);
                    ...
                    return AVERROR_INVALIDDATA;
                } else if (last_pic_droppable != h->droppable) {
                    ... AVERROR_PATCHWELCOME ...
                }
            }
        }
    }
```
A valid complementary pair requires **opposite parities** and **same frame_num** and **same droppable status**.

**Phase B - allocation decision (`h264_slice.c:1614-1673`):**
```c
    if (h->first_field) {
        if (!FIELD_PICTURE(h) || h->picture_structure == last_pic_structure) {
            /* Previous field is unmatched */
            h->missing_fields ++;
            h->cur_pic_ptr = NULL;
            h->first_field = FIELD_PICTURE(h);
        } else {
            h->missing_fields = 0;
            if (h->cur_pic_ptr->frame_num != h->poc.frame_num) {
                ff_thread_report_progress(&h->cur_pic_ptr->tf, INT_MAX,
                                          h->picture_structure==PICT_BOTTOM_FIELD);
                h->first_field = 1;
                h->cur_pic_ptr = NULL;
            } else if (h->cur_pic_ptr->reference & DELAYED_PIC_REF) {
                /* already output; cannot draw into it */
                h->first_field = 1;
                h->cur_pic_ptr = NULL;
            } else {
                /* Second field in complementary pair */
                h->first_field = 0;
            }
        }
    } else {
        /* Frame or first field in a potentially complementary pair */
        h->first_field = FIELD_PICTURE(h);
    }

    if (!FIELD_PICTURE(h) || h->first_field) {
        if (h264_frame_start(h) < 0) { ... }        // NEW picture allocated
    } else {
        /* SECOND FIELD: reuse existing cur_pic_ptr, NO new allocation */
        int field = h->picture_structure == PICT_BOTTOM_FIELD;  // 0=top, 1=bottom
        release_unused_pictures(h, 0);
        h->cur_pic_ptr->tf.owner[field] = h->avctx;
        if (CONFIG_ERROR_RESILIENCE)
            ff_h264_set_erpic(&h->er.cur_pic, NULL);
    }

    /* Field-dependent neighbor initialization */
    if (FIELD_PICTURE(h)) {
        for(i = (h->picture_structure == PICT_BOTTOM_FIELD); i<h->mb_height; i++)
            memset(h->slice_table + i*h->mb_stride, -1,
                   (h->mb_stride - (i+1==h->mb_height)) * sizeof(*h->slice_table));
    } else {
        memset(h->slice_table, -1,
            (h->mb_height * h->mb_stride - 1) * sizeof(*h->slice_table));
    }
```

This is the key: for the second field, the **same `H264Picture`** is kept. Only `tf.owner[field]` and `slice_table` (invalidated for rows of the new parity) change. Also note `ret = ff_h264_init_poc(...)` is called for every field at `h264_slice.c:1676`.

**Phase C - output property export only after pair completes (`h264_slice.c:1725-1734`):**
```c
    if (!FIELD_PICTURE(h) || !h->first_field || h->missing_fields > 1) {
        ret = h264_export_frame_props(h);
        ...
        ret = h264_select_output_frame(h);
        ...
    }
```
Frame properties and output selection happen only for a frame, or for the **second field** of a pair (`!h->first_field`). For the first field, output is deferred until its pair arrives (or until end-of-stream via `finalize_frame` duplication).

### NAL dispatch - `ff_h264_queue_decode_slice()` (`h264_slice.c:2178-2310`)
```c
    if (sl->first_mb_addr == 0) { // FIXME better field boundary detection
        if (h->current_slice) {
            // this slice starts a new field
            ...
            if (h->cur_pic_ptr && FIELD_PICTURE(h) && h->first_field) {
                ret = ff_h264_field_end(h, h->slice_ctx, 1);
                ...
            }
            ...
        }
        if (!h->first_field) {
            if (h->cur_pic_ptr && !h->droppable) {
                ff_thread_report_progress(&h->cur_pic_ptr->tf, INT_MAX,
                                          h->picture_structure == PICT_BOTTOM_FIELD);
            }
            h->cur_pic_ptr = NULL;
        }
    }
    ...
    if (h->current_slice == 0) {
        ret = h264_field_start(h, sl, nal, first_slice);
    } else {
        if (h->picture_structure != sl->picture_structure || ...) {
            av_log(..., "Changing field mode (%d -> %d) between slices is not allowed\n", ...);
            return AVERROR_INVALIDDATA;
        }
    }
```

### Field completion - `ff_h264_field_end()` (`h264_picture.c:190-231`)
```c
int ff_h264_field_end(H264Context *h, H264SliceContext *sl, int in_setup)
{
    AVCodecContext *const avctx = h->avctx;
    H264Picture *cur = h->cur_pic_ptr;
    int err = 0;
    h->mb_y = 0;

    if (in_setup || !(avctx->active_thread_type & FF_THREAD_FRAME)) {
        if (!h->droppable) {
            err = ff_h264_execute_ref_pic_marking(h);
            h->poc.prev_poc_msb = h->poc.poc_msb;
            h->poc.prev_poc_lsb = h->poc.poc_lsb;
        }
        h->poc.prev_frame_num_offset = h->poc.frame_num_offset;
        h->poc.prev_frame_num        = h->poc.frame_num;
    }
    ...
    if (!in_setup && !h->droppable)
        ff_thread_report_progress(&cur->tf, INT_MAX,
                                  h->picture_structure == PICT_BOTTOM_FIELD);

    h->current_slice = 0;
    return err;
}
```
POC state (`prev_poc_msb`, `prev_poc_lsb`, `prev_frame_num`) is updated **per field** via `ff_h264_field_end()`. This is essential: the bottom field's POC MSB calculation uses the top field's LSB as `prev_poc_lsb`.

---

## 3. POC computation for field pictures

### `ff_h264_init_poc()` (`h264_parse.c:280-345`)

Full function:
```c
int ff_h264_init_poc(int pic_field_poc[2], int *pic_poc,
                     const SPS *sps, H264POCContext *pc,
                     int picture_structure, int nal_ref_idc)
{
    const int max_frame_num = 1 << sps->log2_max_frame_num;
    int64_t field_poc[2];

    pc->frame_num_offset = pc->prev_frame_num_offset;
    if (pc->frame_num < pc->prev_frame_num)
        pc->frame_num_offset += max_frame_num;

    if (sps->poc_type == 0) {
        const int max_poc_lsb = 1 << sps->log2_max_poc_lsb;
        if (pc->prev_poc_lsb < 0)
            pc->prev_poc_lsb =  pc->poc_lsb;

        if (pc->poc_lsb < pc->prev_poc_lsb &&
            pc->prev_poc_lsb - pc->poc_lsb >= max_poc_lsb / 2)
            pc->poc_msb = pc->prev_poc_msb + max_poc_lsb;
        else if (pc->poc_lsb > pc->prev_poc_lsb &&
                 pc->prev_poc_lsb - pc->poc_lsb < -max_poc_lsb / 2)
            pc->poc_msb = pc->prev_poc_msb - max_poc_lsb;
        else
            pc->poc_msb = pc->prev_poc_msb;
        field_poc[0] =
        field_poc[1] = pc->poc_msb + pc->poc_lsb;
        if (picture_structure == PICT_FRAME)
            field_poc[1] += pc->delta_poc_bottom;
    } else if (sps->poc_type == 1) {
        ... expectedpoc computation ...
        field_poc[0] = expectedpoc + pc->delta_poc[0];
        field_poc[1] = field_poc[0] + sps->offset_for_top_to_bottom_field;
        if (picture_structure == PICT_FRAME)
            field_poc[1] += pc->delta_poc[1];
    } else {
        int poc = 2 * (pc->frame_num_offset + pc->frame_num);
        if (!nal_ref_idc)
            poc--;
        field_poc[0] = poc;
        field_poc[1] = poc;
    }

    if (   field_poc[0] != (int)field_poc[0]
        || field_poc[1] != (int)field_poc[1])
        return AVERROR_INVALIDDATA;

    /* *** THE CRITICAL FIELD-SPECIFIC PART *** */
    if (picture_structure != PICT_BOTTOM_FIELD)
        pic_field_poc[0] = field_poc[0];
    if (picture_structure != PICT_TOP_FIELD)
        pic_field_poc[1] = field_poc[1];
    *pic_poc = FFMIN(pic_field_poc[0], pic_field_poc[1]);

    return 0;
}
```

Explanation of the field logic:
- `pic_field_poc[]` is `h->cur_pic_ptr->field_poc[]`, initialized to `INT_MAX` for a new picture.
- Top field: only `field_poc[0]` is written; `field_poc[1]` stays at `INT_MAX` (from top field's allocation) until the bottom field arrives.
- Bottom field: only `field_poc[1]` is written; `field_poc[0]` retains the top field's value.
- Frame: both are written.
- `pic_poc` (the frame POC used for reordering) is `FFMIN(top, bottom)` - at first-field time this is just the decoded field's POC because the other slot is `INT_MAX`.
- For type 0 field pairs: each field's own `poc_lsb` from its slice header is used; a TFF pair has top lsb=X, bottom lsb=X+1, giving `field_poc = [X, X+1]` and `top_field_first = 1`. A BFF pair has bottom lsb=X, top lsb=X+1, giving `field_poc = [X+1, X]` and `top_field_first = 0`.

### Call site (`h264_slice.c:1676-1678`)
```c
    ret = ff_h264_init_poc(h->cur_pic_ptr->field_poc, &h->cur_pic_ptr->poc,
                     h->ps.sps, &h->poc, h->picture_structure, nal->ref_idc);
```

### `top_field_first` derivation (`h264_slice.c:1265-1285`, `h264_export_frame_props`)
```c
    if (cur->field_poc[0] != cur->field_poc[1]) {
        /* Derive top_field_first from field pocs. */
        top_field_first = (cur->field_poc[0] < cur->field_poc[1]);
    } else {
        if (sps->pic_struct_present_flag && h->sei.picture_timing.present) {
            if (h->sei.picture_timing.pic_struct == H264_SEI_PIC_STRUCT_TOP_BOTTOM ||
                h->sei.picture_timing.pic_struct == H264_SEI_PIC_STRUCT_TOP_BOTTOM_TOP)
                top_field_first = 1;
        } else if (interlaced_frame) {
            top_field_first = 1;   // default to TFF when interlaced
        }
    }
    out->flags |= (AV_FRAME_FLAG_INTERLACED * interlaced_frame) |
                  (AV_FRAME_FLAG_TOP_FIELD_FIRST * top_field_first);
```
`interlaced_frame` is set from `FIELD_OR_MBAFF_PICTURE(h)` unless picture timing SEI overrides.

---

## 4. Reference picture lists and complementary field pairs

### `h264_refs.c:39-47` - `pic_as_field()`: turn a picture into a field ref
```c
static void pic_as_field(H264Ref *pic, const int parity)
{
    for (int i = 0; i < FF_ARRAY_ELEMS(pic->data); ++i) {
        if (parity == PICT_BOTTOM_FIELD)
            pic->data[i]   += pic->linesize[i];
        pic->reference      = parity;
        pic->linesize[i] *= 2;
    }
    pic->poc = pic->parent->field_poc[parity == PICT_BOTTOM_FIELD];
}
```
Bottom field pointer = base + 1 line; strides doubled; POC from `field_poc[0 or 1]`.

### `h264_refs.c:49-75` - `ref_from_h264pic()` + `split_field_copy()`
```c
static void ref_from_h264pic(H264Ref *dst, const H264Picture *src)
{
    memcpy(dst->data,     src->f->data,     sizeof(dst->data));
    memcpy(dst->linesize, src->f->linesize, sizeof(dst->linesize));
    dst->reference = src->reference;
    dst->poc       = src->poc;
    dst->pic_id    = src->pic_id;
    dst->parent = src;
}

static int split_field_copy(H264Ref *dest, const H264Picture *src,
                            int parity, int id_add)
{
    int match = !!(src->reference & parity);
    if (match) {
        ref_from_h264pic(dest, src);
        if (parity != PICT_FRAME) {
            pic_as_field(dest, parity);
            dest->pic_id *= 2;
            dest->pic_id += id_add;
        }
    }
    return match;
}
```
`match = src->reference & parity` uses the reference bitmask. Field pic_ids are `2*frame_num` (bottom, id_add=0) or `2*frame_num+1` (top, id_add=1).

### `h264_refs.c:77-94` - `build_def_list()`: interleave complementary fields
```c
static int build_def_list(H264Ref *def, int def_len,
                          H264Picture * const *in, int len, int is_long, int sel)
{
    int  i[2] = { 0 };
    int index = 0;

    while (i[0] < len || i[1] < len) {
        while (i[0] < len && !(in[i[0]] && (in[i[0]]->reference & sel)))
            i[0]++;
        while (i[1] < len && !(in[i[1]] && (in[i[1]]->reference & (sel ^ 3))))
            i[1]++;
        if (i[0] < len) {
            in[i[0]]->pic_id = is_long ? i[0] : in[i[0]]->frame_num;
            split_field_copy(&def[index++], in[i[0]++], sel, 1);
        }
        if (i[1] < len) {
            in[i[1]]->pic_id = is_long ? i[1] : in[i[1]]->frame_num;
            split_field_copy(&def[index++], in[i[1]++], sel ^ 3, 0);
        }
    }
    return index;
}
```
`sel` = `h->picture_structure` (1=top, 2=bottom for fields). `sel^3` toggles the parity (1^3=2, 2^3=1). The two indices walk the DPB in parallel to interleave the current-parity field with its complement (complementary field pairs are adjacent in the list).

### `h264_refs.c:133-201` - `h264_initialise_ref_list()`
```c
    if (sl->slice_type_nos == AV_PICTURE_TYPE_B) {
        H264Picture *sorted[32];
        int cur_poc;
        int lens[2];

        if (FIELD_PICTURE(h))
            cur_poc = h->cur_pic_ptr->field_poc[h->picture_structure == PICT_BOTTOM_FIELD];
        else
            cur_poc = h->cur_pic_ptr->poc;

        for (int list = 0; list < 2; list++) {
            len  = add_sorted(sorted,       h->short_ref, h->short_ref_count, cur_poc, 1 ^ list);
            len += add_sorted(sorted + len, h->short_ref, h->short_ref_count, cur_poc, 0 ^ list);
            ...
            len  = build_def_list(sl->ref_list[list], ..., sorted, len, 0, h->picture_structure);
            len += build_def_list(sl->ref_list[list] + len, ..., h->long_ref, 16, 1, h->picture_structure);
            ...
        }
        // avoid identical first entries in L0/L1
        if (lens[0] == lens[1] && lens[1] > 1) {
            ... FFSWAP(H264Ref, sl->ref_list[1][0], sl->ref_list[1][1]); ...
        }
    } else {
        len  = build_def_list(sl->ref_list[0], ..., h->short_ref, h->short_ref_count, 0, h->picture_structure);
        len += build_def_list(sl->ref_list[0] + len, ..., h->long_ref, 16, 1, h->picture_structure);
        ...
    }
    for (int i = 0; i < sl->list_count; i++)
        h->default_ref[i] = sl->ref_list[i][0];
```
B-list sorting uses the **current field's POC** when in a field picture.

### `h264_refs.c:255-268` - `pic_num_extract()`: field pic_num -> frame_num + structure
```c
static int pic_num_extract(const H264Context *h, int pic_num, int *structure)
{
    *structure = h->picture_structure;
    if (FIELD_PICTURE(h)) {
        if (!(pic_num & 1))
            /* opposite field */
            *structure ^= PICT_FRAME;
        pic_num >>= 1;
    }
    return pic_num;
}
```
For field pictures: odd pic_num = current field, even pic_num = opposite field (`structure ^= 3` flips parity). Then `pic_num >>= 1` converts to frame_num.

### `h264_refs.c:268-290` - `h264_fill_mbaff_ref_list()`: MBAFF only
Populates `sl->ref_list[list][16+2*i]` with top field (`linesize<<1`, `poc=field_poc[0]`) and `+1` with bottom field (`data += f->linesize`, `poc=field_poc[1]`). Relevant only for MBAFF, not PAFF, but shows the same field addressing principle.

### `h264_refs.c:292-429` - `ff_h264_build_ref_list()`
Ref list modification loop; for each modification:
```c
                frame_num = pic_num_extract(h, pred, &pic_structure);
                ...
                if (ref->frame_num == frame_num &&
                    (ref->reference & pic_structure))
                    break;
                ...
                ref_from_h264pic(&sl->ref_list[list][index], ref);
                if (FIELD_PICTURE(h)) {
                    pic_as_field(&sl->ref_list[list][index], pic_structure);
                }
```
After reorder:
```c
            if (   !sl->ref_list[list][index].parent
                || (!FIELD_PICTURE(h) && (sl->ref_list[list][index].reference&3) != 3)) {
                ... fallback to h->default_ref[list] ...
            }
```
Frame pictures require a full frame ref (`(reference&3)==3`), field pictures accept single-field refs.

### `h264_refs.c:588-608` - `generate_sliding_window_mmcos()`: both fields of a frame
```c
    if (h->short_ref_count &&
        h->long_ref_count + h->short_ref_count >= h->ps.sps->ref_frame_count &&
        !(FIELD_PICTURE(h) && !h->first_field && h->cur_pic_ptr->reference)) {
        mmco[0].opcode        = MMCO_SHORT2UNUSED;
        mmco[0].short_pic_num = h->short_ref[h->short_ref_count - 1]->frame_num;
        nb_mmco               = 1;
        if (FIELD_PICTURE(h)) {
            mmco[0].short_pic_num *= 2;
            mmco[1].opcode         = MMCO_SHORT2UNUSED;
            mmco[1].short_pic_num  = mmco[0].short_pic_num + 1;
            nb_mmco                = 2;
        }
    }
```
Field mode generates two MMCOs (pic_num*2 and pic_num*2+1) to unref both fields.

### `h264_refs.c:478-490` - `unreference_pic()` with field mask
```c
static inline int unreference_pic(H264Context *h, H264Picture *pic, int refmask)
{
    if (pic->reference &= refmask) {
        return 0;
    } else {
        for (int i = 0; h->delayed_pic[i]; i++)
            if(pic == h->delayed_pic[i]){
                pic->reference = DELAYED_PIC_REF;
                break;
            }
        return 1;
    }
}
```

### `h264_refs.c:610-825` - `ff_h264_execute_ref_pic_marking()`
MMCO handling passes `structure ^ PICT_FRAME` as refmask to `remove_short`/`remove_long`, so unreferencing one field leaves the other:
```c
        case MMCO_SHORT2UNUSED:
        case MMCO_SHORT2LONG: {
            int structure, j;
            int frame_num = pic_num_extract(h, mmco[i].short_pic_num, &structure);
            ...
            remove_short(h, frame_num, structure ^ PICT_FRAME);
            ...
        }
        case MMCO_LONG2UNUSED: {
            int structure, j = pic_num_extract(h, mmco[i].long_arg, &structure);
            ...
            remove_long(h, j, structure ^ PICT_FRAME);
            ...
        }
        case MMCO_LONG:
            ...
            h->cur_pic_ptr->reference |= h->picture_structure;
            current_ref_assigned = 1;
            break;
```
`reference |= picture_structure` marks the specific field of the current picture as reference.

**Complementary field pair insertion (`h264_refs.c:738-766`):**
```c
    if (!current_ref_assigned) {
        /* Second field of complementary field pair; the first field of
         * which is already referenced. If short referenced, it
         * should be first entry in short_ref. If not, it must exist
         * in long_ref; trying to put it on the short list here is an
         * error in the encoded bit stream (ref: 7.4.3.3, NOTE 2 and 3).
         */
        if (h->short_ref_count && h->short_ref[0] == h->cur_pic_ptr) {
            /* Just mark the second field valid */
            h->cur_pic_ptr->reference |= h->picture_structure;
        } else if (h->cur_pic_ptr->long_ref) {
            av_log(..., "illegal short term reference assignment for second field in complementary field pair ...");
            err = AVERROR_INVALIDDATA;
        } else {
            ...
            h->short_ref[0] = h->cur_pic_ptr;
            h->short_ref_count++;
            h->cur_pic_ptr->reference |= h->picture_structure;
        }
    }
```

### `h264_refs.c:832-884` - `ff_h264_decode_ref_pic_marking()`
Field-related point: `MMCO_LONG2UNUSED` with `long_arg >= 16` is allowed for field pictures (`&& !(opcode == MMCO_LONG2UNUSED && FIELD_PICTURE(sl))`).

---

## 5. Output frame assembly - which rows, linesize, height

### Storage layout (confirmed by `h264_mb_template.c:40-96`)
```c
    dest_y  = h->cur_pic.f->data[0] + ((mb_x << PIXEL_SHIFT)     + mb_y * sl->linesize)  * 16;
    dest_cb = h->cur_pic.f->data[1] +  (mb_x << PIXEL_SHIFT) * 8 + mb_y * sl->uvlinesize * block_h;
    dest_cr = h->cur_pic.f->data[2] +  (mb_x << PIXEL_SHIFT) * 8 + mb_y * sl->uvlinesize * block_h;
    ...
    if (!SIMPLE && MB_FIELD(sl)) {
        linesize     = sl->mb_linesize = sl->linesize * 2;
        uvlinesize   = sl->mb_uvlinesize = sl->uvlinesize * 2;
        block_offset = &h->block_offset[48];    // field block offsets
        if (mb_y & 1) { // bottom field (odd frame MB rows)
            dest_y  -= sl->linesize * 15;
            dest_cb -= sl->uvlinesize * (block_h - 1);
            dest_cr -= sl->uvlinesize * (block_h - 1);
        }
    } else {
        linesize   = sl->mb_linesize   = sl->linesize;
        uvlinesize = sl->mb_uvlinesize = sl->uvlinesize;
    }
```
MB row mapping from `h264_slice.c:1971-1977` (`h264_slice_init`):
```c
    sl->resync_mb_x = sl->mb_x =  sl->first_mb_addr % h->mb_width;
    sl->resync_mb_y = sl->mb_y = (sl->first_mb_addr / h->mb_width) <<
                                 FIELD_OR_MBAFF_PICTURE(h);
    if (h->picture_structure == PICT_BOTTOM_FIELD)
        sl->resync_mb_y = sl->mb_y = sl->mb_y + 1;
```
`first_mb_addr` is in **field MB units**; `<< 1` maps to frame MB row. Top field = even frame MB rows (0,2,4,...), bottom field = odd frame MB rows (1,3,5,...). The `dest_y -= linesize*15` correction for bottom field moves the pointer from frame row `16*1=16` back to frame row `1`. With `linesize*2`, subsequent field rows are at frame rows 1,3,5,...,31. So:

| Component | Top field rows | Bottom field rows | Stride used |
|-----------|---------------|-------------------|-------------|
| Y plane   | 0, 2, 4, ...   | 1, 3, 5, ...      | `linesize*2` |
| U/V (4:2:0) | 0, 2, 4, ... (chroma rows) | 1, 3, 5, ... | `uvlinesize*2` |

Buffer height is the **full frame height** (`h->height = 16*h->mb_height`, where `sps->mb_height = (pic_height_in_mbs)* (2 - frame_mbs_only_flag)` from `h264_ps.c:449-457`).

### `decode_finish_row()` (`h264_slice.c:2642-2672`)
```c
static void decode_finish_row(const H264Context *h, H264SliceContext *sl)
{
    int top            = 16 * (sl->mb_y      >> FIELD_PICTURE(h));
    int pic_height     = 16 *  h->mb_height >> FIELD_PICTURE(h);
    int height         =  16      << FRAME_MBAFF(h);
    int deblock_border = (16 + 4) << FRAME_MBAFF(h);
    ...
    ff_h264_draw_horiz_band(h, sl, top, height);
    ...
    ff_thread_report_progress(&h->cur_pic_ptr->tf, top + height - 1,
                              h->picture_structure == PICT_BOTTOM_FIELD);
}
```
For fields: `top` and `pic_height` are in **field line coordinates** (mb_y>>1, mb_height>>1). Thread progress is reported with field index 0 (top) or 1 (bottom).

### `ff_h264_draw_horiz_band()` (`h264dec.c:104-140`)
```c
void ff_h264_draw_horiz_band(const H264Context *h, H264SliceContext *sl,
                             int y, int height)
{
    ...
    int field_pic = h->picture_structure != PICT_FRAME;
    if (!avctx->draw_horiz_band)
        return;
    if (field_pic && h->first_field && !(avctx->slice_flags & SLICE_FLAG_ALLOW_FIELD))
        return;
    if (field_pic) {
        height <<= 1;
        y      <<= 1;
    }
    height = FFMIN(height, avctx->height - y);
    ...
    offset[0] = y * src->linesize[0];
    offset[1] =
    offset[2] = (y >> vshift) * src->linesize[1];
    ...
    avctx->draw_horiz_band(avctx, src, offset,
                           y, h->picture_structure, height);
}
```
Field coordinates are doubled back to frame coordinates when exporting horizontal bands.

### `finalize_frame()` - unpaired field duplication (`h264dec.c:1010-1055`)
```c
static int finalize_frame(H264Context *h, AVFrame *dst, H264Picture *out, int *got_frame)
{
    int ret;

    if (((h->avctx->flags & AV_CODEC_FLAG_OUTPUT_CORRUPT) ||
         (h->avctx->flags2 & AV_CODEC_FLAG2_SHOW_ALL) ||
         out->recovered)) {

        if (h->skip_gray > 0 && h->non_gray && out->gray &&
            !(h->avctx->flags2 & AV_CODEC_FLAG2_SHOW_ALL))
            return 0;

        if (!h->avctx->hwaccel && !ff_decode_skip_all_pixels(h->avctx) &&
            (out->field_poc[0] == INT_MAX ||
             out->field_poc[1] == INT_MAX)
           ) {
            int p;
            AVFrame *f = out->f;
            int field = out->field_poc[0] == INT_MAX;   // 1 if top missing, 0 if bottom missing
            uint8_t *dst_data[4];
            int linesizes[4];
            const uint8_t *src_data[4];

            av_log(h->avctx, AV_LOG_DEBUG, "Duplicating field %d to fill missing\n", field);

            for (p = 0; p<4; p++) {
                dst_data[p] = FF_PTR_ADD(f->data[p], (field^1)*f->linesize[p]);
                src_data[p] = FF_PTR_ADD(f->data[p],  field   *f->linesize[p]);
                linesizes[p] = 2*f->linesize[p];
            }

            av_image_copy(dst_data, linesizes, src_data, linesizes,
                          f->format, f->width, f->height>>1);
        }

        ret = output_frame(h, dst, out);
        ...
        *got_frame = 1;
        ...
    }
    return 0;
}
```
This is the ONLY explicit "field combining" step: when a picture has a missing field (one `field_poc[] == INT_MAX`), the decoded field is **duplicated** into the other field's rows. For a proper complementary pair, both fields are already present in the buffer and no copying happens. The output `AVFrame` is the full-height frame with both fields, with `AV_FRAME_FLAG_INTERLACED` and `AV_FRAME_FLAG_TOP_FIELD_FIRST` set by `h264_export_frame_props`.

### `output_frame()` (`h264dec.c:942-...`)
Simply `av_frame_ref(dst, srcp->f)` - no row manipulation. The whole full-height buffer is the output frame.

### Frame output selection (`h264_slice.c:1323-1419`, `h264_select_output_frame()`)
Adds `cur` to `h->delayed_pic[]`, sets `DELAYED_PIC_REF` if non-reference, reorders by `poc`, and sets `h->next_output_pic` for the lowest-POC frame. Called only for completed frames/pairs (see `h264_field_start` Phase C). `finalize_frame()` in `h264dec.c:1162` actually emits `h->next_output_pic`.

---

## Summary: PAFF decode flow (porting checklist)

1. **Slice header**: read `field_pic_flag` (if !frame_mbs_only), then `bottom_field_flag`; `picture_structure = 1+bottom_field_flag`; `mb_field_decoding_flag=1`; double `curr_pic_num`/`max_pic_num`.
2. **Field start**: if `first_field` was set and new field has opposite parity + same frame_num + same droppable status -> it's the second field: reuse `cur_pic_ptr`, set `first_field=0`, set `tf.owner[bottom]=ctx`, clear slice_table for new parity rows. Otherwise allocate a new picture via `h264_frame_start` (full-height frame buffer), set `field_picture`, `reference = picture_structure` bitmask, `field_poc=[INT_MAX,INT_MAX]`, `first_field=FIELD_PICTURE`.
3. **POC**: call `ff_h264_init_poc`; it writes only the slot for the current field, leaving the other `INT_MAX`; frame POC = min of both. Update POC MSB/LSB state after each field via `ff_h264_field_end`.
4. **Ref lists**: `build_def_list` with `sel=picture_structure` interleaves current/opposite parity; `split_field_copy`/`pic_as_field` produce field refs with `data+=linesize` for bottom, `linesize*=2`, `poc=field_poc[parity]`; `pic_num_extract` strips the field bit from pic_num.
5. **MB addressing**: field MB `mb_y` = `(first_mb_addr/mb_width)<<1` (+1 for bottom); `dest_y = data[0] + 16*(mb_x + mb_y*linesize)`; for field MBs use `linesize*2` and offset `dest_y -= 15*linesize` when bottom (odd mb_y).
6. **Output**: both fields share one full-height buffer; top in even rows, bottom in odd rows. Export interlaced flags after pair completes. For an unpaired field, duplicate it into the missing field's rows with `av_image_copy` using doubled strides and half height.

---

## Appendix A: wedeo Architecture Map (from subagent exploration)

### Field flags parsed but ignored
- `slice.rs:126-128`: `field_pic_flag`, `bottom_field_flag` in `SliceHeader`
- `slice.rs:505-514`: parsing (only when `!frame_mbs_only_flag`)
- Never read by `decoder.rs` (confirmed exhaustive search)

### Critical bug: `is_mbaff` derived from wrong flag
- `decoder.rs:1601/2011`: `let is_mbaff = !sps.frame_mbs_only_flag;` (before fix)
- `mb.rs:612`: `is_mbaff: !sps.frame_mbs_only_flag`
- PAFF streams (`frame_mbs_only_flag=0, mb_aff=0`) incorrectly route through MBAFF pair loops,
  which read non-existent `mb_field_decoding_flag` bits → bitstream desync.
- **Fix**: use `!sps.frame_mbs_only_flag && sps.mb_aff`

### Decode flow
`send_packet` (2697) → `process_nal` (749) → slice header parse (781) →
`first_mb==0` new-picture gate (799-802) → FDC creation (825) →
POC compute (850-861) → DPB placeholder (863-887) → slice snapshot (925-1076) →
`finish_current_frame` (1097) → thread pool dispatch (1229-1248) →
`decode_frame_sequential` (thread_pool.rs:207-257) → `complete_in_flight` (1307-1546) →
`fdc_to_frame` (2556-2646) → `delayed_pics` → `output_queue`

### FDC geometry
`FrameDecodeContext::new` (mb.rs:478-620) always allocates full-frame height.
`sps.mb_height = pic_height_in_map_units * (2 - frame_mbs_only_flag)` = full frame MB rows.
Existing `luma_block_offset(..., mb_field: bool)` (mb.rs:771-793) already implements
doubled-stride field addressing: `pair_base_y = (mb_y & !1)*16`, `field_offset = (mb_y&1)*stride`.

### DPB / output
- `DpbEntry` (dpb.rs:35-66) has no field parity metadata
- `build_ref_list_p/b` (refs.rs:54-231) use frame-only `pic_num = frame_num`, `MaxPicNum = 2^log2_max_frame_num`
- Output ordering via `H264Decoder.delayed_pics: Vec<(poc, Frame, bool)>`; DPB never outputs directly

### Field POC reference implementation
`formats/wedeo-format-h264/src/lib.rs:213-311` `init_poc()` already implements:
- `picture_structure = 1 + bottom_field_flag` (FFmpeg PICT_TOP=1, PICT_BOTTOM=2, PICT_FRAME=3)
- POC type 0: `field_poc[0]=field_poc[1]=msb+lsb`, `+= delta_bottom` only for frames
- `pic_field_poc[0]` set only for top/frame, `[1]` only for bottom/frame
- Frame POC = min of the two

---

## Appendix B: Failing Vector Classification

All 20 interlaced FRext vectors analyzed with `-v debug -debug pict`:

| Vector | SPS | T/B/F slices | Pairs | Pkts/Frames | Class |
|---|---|---|---|---|---|
| Hi422FREXT16 | PIC-AFF | 5/5/0 | 5 | 10/5 | PAFF only |
| Hi422FREXT17 | MB-AFF | 0/0/5 | 0 | 5/5 | MBAFF only |
| Hi422FREXT18 | PIC-AFF | 35/35/68 | 35 | 138/103 | PAFF + frame mix |
| Hi422FREXT19 | MB-AFF | 0/0/103 | 0 | 103/103 | MBAFF only |
| Freh7_B | PIC-AFF | 100/100/0 | 100 | 200/100 | PAFF only |
| FRExt2_Panasonic | MB-AFF | 7/7/8 | 7 | 22/15 | PAFF + frame mix |
| FRExt4_Panasonic | MB-AFF | 5/5/5 | 5 | 15/10 | PAFF + frame mix |
| HCAFF1_HHI | PIC-AFF | 4/4/6 | 4 | 14/10 | PAFF + frame mix |
| HCAMFF1_HHI | MB-AFF | 0/0/10 | 0 | 10/10 | MBAFF only |
| FREXT01/02_JVC | MB-AFF | 0/0/16 | 0 | 16/16 | MBAFF only |
| HCHP3_HHI_A | MB-AFF | 65/65/185 | 65 | 315/250 | PAFF + frame mix |
| HPCAFL/NL, HPCVFL/NL | PIC-AFF | 300/300/0 | 300 | 600/300 | PAFF only |
| HPCAMAPALQ | MB-AFF | 158/158/142 | 158 | 458/300 | PAFF + frame mix |
| HVLCFI0 | PIC-AFF | 19/19/0 | 19 | 38/19 | PAFF only |
| HVLCMFF0/HVLCPFF0 | mixed | 11/11/8 | 11 | 30/19 | PAFF + frame mix |

Key facts:
- All POC type 0, top-field first, complementary pairs (equal T/B counts, same frame_num).
- 5 vectors are MBAFF-only (need no PAFF work, just MBAFF pixel fixes).
- 5 vectors are pure PAFF; 10 are mixed PAFF+frame.
- Even MB-AFF SPS streams can contain field slices → per-slice `field_pic_flag` is authoritative.
- Field ref lists use ~2x entries vs frame (field references, `pic_num` doubled).

---

## Appendix C: Implementation Attempt (2026-11-26) - Status: INCOMPLETE

A partial implementation was attempted on branch `main` (commits after `224baa8`) with:
1. `is_mbaff = !frame_mbs_only_flag && sps.mb_aff` ✅ CORRECT - keep this
2. PAFF top/bottom field pairing in `process_nal` (reuse FDC for bottom field) ✅ conceptually correct
3. Field MB address mapping (`mb_y = fy*2 + bottom`) ✅ correct
4. Field POC slots `[top, bottom]` with min ✅ correct
5. CABAC field neighbor computation ✅ correct
6. Field-view MC for self-references ⚠️ partial - wrong pixels, caused hangs

**Why it failed**: field reference lists / `pic_num` semantics were NOT implemented
(`build_ref_list_p/b` still frame-only). The bottom field's ref list was empty, then
after forcing the placeholder to ShortTerm, MC self-referenced with wrong field parity
and hung on `wait_for_row`. Full field ref list implementation (FFmpeg `h264_refs.c`
`pic_as_field`, `pic_num_extract`, `build_def_list`) is required before pixel parity
can be achieved.

**Recommendation for next session**: implement in this order:
1. Add `field_parity: u8` to `DpbEntry` (0=top, 1=bottom, 3=frame/both).
2. Field-aware `pic_num`: `2*FrameNum + parity`, `MaxPicNum = 2^(log2_max_frame_num+1)`.
3. In `build_ref_list_p/b`, for field pictures interleave `sel` and `sel^3` fields.
4. When MC reads a field reference, apply `pic_as_field`: `data += linesize` for bottom,
   `linesize *= 2`, `poc = field_poc[parity]`.
5. Then re-enable PAFF pairing and field-view MC.
6. CAVLC path also needs field-aware `NeighborContext` (top_nz per field row).

---

## Appendix D: Infrastructure landed 2026-11-26

The following PAFF **infrastructure** is implemented and verified not to regress
frame-only decoding (all 39 progressive FRext matches unchanged):

1. `DpbEntry.ref_mask` (bit0=top, bit1=bottom, 3=frame) and `DpbEntry.field_poc[2]`.
2. `RefListEntry { dpb_idx, parity, pic_num }` - reference-list entries now carry
   field parity. `build_ref_list_p/b` take a `structure` argument and expand
   short/long-term refs into field entries with doubled `pic_num` for field
   pictures (`expand_short_term`, `expand_long_term`, `find_ref_entry`).
3. `RefPic { pic, parity, poc, frame_num }` - a reference-list entry with field
   parity, Deref to `SharedPicture`. All MC / reconstruction paths take
   `&[RefPic]` instead of `&[Arc<SharedPicture>]`.
4. Unit tests for field interleaving, top-only refs, and frame-ref requirements.

**Missing at that time (2026-11-26; do not re-attempt the old partial patch
without ground truth):**
- PAFF top/bottom pairing in `process_nal` (bottom field must reuse the top
  field's FDC and DPB slot, with one combined output frame).
- Field MB row addressing in the CABAC/CAVLC loops (`mb_y = 2*field_y + parity`).
- Field-view MC (`pic_as_field`: bottom data offset by one line, doubled stride,
  field-coordinate `ref_y`) in `apply_mc_partition` and `reconstruct_inter_mb_hires`.
- Field-aware deblocking.

Appendix Q later implemented these as a bounded step with trade-offs; see the
status summary above and the Appendix Q notes for what remains.

A partial implementation of the missing pieces was tried and reverted: it
produced ~140k differing pixels per frame and hung on
`Hi422FREXT16_SONY_A`. The likely remaining defect is in CABAC neighbor context
for field rows; extract `mb_type` / neighbor state from single-thread debug
FFmpeg before theorising further.

---

## Appendix E: FFmpeg vs Wedeo PAFF ground truth (2026-11-26, measured)

Source: `/tmp/ai-agent/PAFF_GROUND_TRUTH.md` (gdb + temporary `trace!` probes, all reverted).
Vector: `/var/cache/zstd5/tmp/freh7_2f.264` (352x288, 22x18 MBs, High/PIC-AFF, CABAC,
POC type 0, top-first complementary I+P field pair). Debug FFmpeg with `-threads 1 -bitexact`.

### Output level
| Decoder | Frames | framecrc |
|---|---|---|
| FFmpeg | **1** | `0x89f2e8d5` |
| Wedeo | 2 | `0x068f1ce5`, `0xdeff31ea` |

~99.9% of Y samples differ against either Wedeo frame.

### FIRST POINT OF DIVERGENCE (slice-header handling, before MB decode)
- FFmpeg: both slices have `first_mb_addr=0`; `h264_field_start` pairs them into
  **one picture** (same `curpic` pointer). `h264_slice_init` maps the top slice to
  `mb_y=0` (`picture_structure=1`) and the bottom slice to `mb_y=1`
  (`picture_structure=2`), both with `mb_field_decoding_flag=1`.
- Wedeo: both slices take the `if hdr.first_mb_in_slice == 0` new-frame gate
  (`decoder.rs:802`), allocate **separate FDCs** (`decoder.rs:828`), and start at
  `mb_y=0` using frame row addressing (`mb_y = mb_addr / mb_width`,
  `decoder.rs:1630/2040`). Two output frames.

### Measured per-MB divergences (post first-divergence)
| Field | MB | FFmpeg | Wedeo |
|---|---|---|---|
| top/I | (1,0) | CABAC `state[68]`=8 | `state[68]`=0 (state[54] still 124) |
| top/I | (0,2) | neighbor `top_xy=0`, `left_xy=0` | `top_idx=0`, `left_idx=None` |
| bottom/P | (0,1) | cbp=239, mb_type=12480 (coded) | cbp=175, mb_type=3 (skip path) |

FFmpeg field-row stride for top neighbors is `mb_stride*2`; Wedeo uses
`mb_width` (frame stride). FFmpeg's `mb_stride=23` (22 MBs + 1 padding).

### Concrete Wedeo locations
- `decoder.rs:802` - `first_mb_in_slice == 0` must not open a new frame for a
  complementary bottom field (FFmpeg `h264_slice.c:1483-1532,1614-1673`).
- `decoder.rs:828` - must reuse `cur_pic_ptr` for the second field.
- `decoder.rs:853-857` - frame-level POC; needs `ff_h264_field_end` equivalent.
- `decoder.rs:899,910` - `build_ref_list_p/b(..., structure=3)` must pass the
  field structure (1/2), not always frame.
- `decoder.rs:1630/2040` - `mb_y = mb_addr / mb_width` must become field addressing:
  `mb_y = (first_mb/mb_width)<<1 (+1 for bottom)`.
- `decoder.rs:1636/1642/1707/1725/2046/2050/2124` - `top_available` must use
  doubled field stride.
- `decoder.rs:2062/2129` - `compute_mbaff_neighbors(..., false, false, ...)` must
  receive field coordinates and `mb_field=true`.

### Conclusion
The pairing/addressing change is the *first* required change, but the earlier
partial patch that only changed row addressing (without pairing + field-view MC)
produced ~140k wrong pixels/frame. Implement in the full order from the main plan
(pairing -> field MB rows -> field-view MC -> field-aware deblock) and re-measure
against these values after each step.

---

## Appendix F: Pairing + field addressing attempt (2026-11-26, reverted)

Patch retained at `/tmp/ai-agent/paff_stage_attempt.patch` (363 lines). Tree is
clean at `ff6feb9`; the patch was NOT committed because pixels remain wrong.

### What was implemented (and what it achieved)
1. `H264Decoder` PAFF state (`current_field_pair`, `current_top_frame_num`,
   `current_top_field_poc`, `current_bottom_seen`). A bottom-field slice with
   `first_mb=0`, equal `frame_num` and a pending top field reuses the FDC instead
   of starting a new frame.
2. POC type 0 bottom field POC = top POC + `delta_pic_order_cnt_bottom`; DPB
   `field_poc[2]` / `ref_mask` updated on the paired entry.
3. `build_ref_list_p/b` receive picture_structure 1/2 for field slices and
   `RefPic::field(...)` preserves field parity.
4. CAVLC/CABAC frame loops use field MB addressing:
   `mb_y = (mb_addr/mb_width)*2 + bottom`, loop over `mb_width * ceil(mb_height/2)`
   MBs, top-neighbour stride `2*mb_width`, and `compute_mbaff_neighbors(...,
   is_mbaff=true, curr_field=true, ...)`.
5. `decode_slice_into` sets `fdc.mb_field = true` and `fdc.is_mbaff = true` for
   field slices.

**Result: frame count went from 2 to 1** for `freh7_2f.264`, but luma parity 0/1
still differ by ~99.9% / ~99.3%, and chroma ~97-99%. No improvement in pixels.

### Measured MB-level state (top I field, temporary trace, reverted)
- MB(0,0): `cbp=239`, `mb_type=0`, `transform_8x8_flag=true`,
  `intra4x4_pred_mode=[2,2,1,1]` (DC/DC/H/H), `luma_coeffs[0]` all zero,
  `luma_8x8_coeffs[0][..8]=[1,1,0,3,0,-1,0,0]`. FFmpeg's cbp is also 239.
- MB(1,2): Wedeo `cbp=15` vs FFmpeg `cbp=47` (entropy divergence persists).
- Bottom P MB(0,1): Wedeo `mb_type=3, cbp=175` (skip) vs FFmpeg coded `cbp=239`.

### Conclusion
Pairing and field addressing alone are insufficient. The first MB's syntax
matches FFmpeg, so the divergence is in **field-mode reconstruction** (intra
prediction / 8x8 intra residual placement with the doubled field stride), not
in bitstream parsing or frame assembly. Disabling deblocking does not help.

### Next step (do NOT repeat this partial patch)
Extract FFmpeg gdb ground truth for the top I field MB(0,0): prediction buffer
after intra prediction / before IDCT, dequantized 8x8 coefficients, and the
field-mode destination stride/offset. Compare against Wedeo's `ctx.pic.y`
immediately after `apply_macroblock` for the same MB. Find WHERE the first
reconstructed pixel diverges before changing code again.

---

## Appendix G: Field-mode intra reconstruction ground truth (2026-11-26, reverted)

Source: `/tmp/ai-agent/PAFF_INTRA_GROUND_TRUTH.md` (gdb + temporary Wedeo probes,
all reverted). Vector `freh7_2f.264`, top I field MB(0,0).

### Measured, in order
1. **Placement matches**: FFmpeg `dest_y - data[0]=0`, `linesize=352`,
   reconstruction stride `704`, `block_offset[0]=0`; Wedeo offset 0,
   `y_stride=352`, block stride 704.
2. **Prediction matches**: FFmpeg `pred8x8l` dir=11 (DC_128) gives eight 128s;
   Wedeo mode=2 with top/left/top-left all 128 also gives eight 128s.
3. **Coefficients diverge**: FFmpeg dequantized block
   `[1480,375,0,375,1232,233,-500,495,...]` (16 nonzeros in first 16) vs
   Wedeo `[296,128,0,540,0,-127,0,0,...]` (6 nonzeros), both at QP 28 / CBP 239.
4. Final row 0 follows: FFmpeg `0 33 214 255 247 251 239 255`, Wedeo
   `151 138 118 118 130 133 120 110`.

### Measured table facts (not yet proven causal)
- FFmpeg `h->field_scan8x8[0..15] = 0,1,2,8,9,3,4,10,16,11,5,6,7,12,17,24`
  (= TRANSPOSE of the raw table), Wedeo `FIELD_SCAN_8X8[0..15] =
  0,8,16,1,9,24,32,17,2,25,40,48,56,33,10,3` (raw, untransposed).
- FFmpeg dequant8[QP28][0..15] = `18944,4800,26240,...`, Wedeo =
  `18944,8160,23040,...` (transposes of each other).
- These two orientation differences cancel for progressive zigzag (which is
  why all progressive vectors pass), but field scan needs a separate check.
- FFmpeg's raw pre-dequant levels were **not extracted**; the values
  `5,5,0,2,7,1,-2,6,-28,...` quoted in the subagent report are *derived* from
  dequantized output and must not be used as evidence.

### Conclusion
The earliest measured divergence is the coefficient block after CABAC residual
decode + dequant, with prediction and placement identical. Extract the raw
bitstream levels from both decoders (FFmpeg `STORE_BLOCK` with `j` in scope,
Wedo before `dequant_8x8`) before changing any scan/dequant logic.

---

## Appendix H: Field CABAC `mb_field` plumbing (2026-11-27, measured)

Gdb ground truth (`/tmp/ai-agent/PAFF_LEVELS_GROUND_TRUTH.md`) on the same
top I field MB(0,0) (using the retained `Freh7_B.264` vector after /tmp was
cleaned) settles the field-mode coefficient divergence:

- FFmpeg's raw pre-dequant levels (40 coefficients) and scan positions were
  extracted directly at the `STORE_BLOCK` multiply site.
- Wedeo with `mb_field` threaded into `decode_mb_cabac` decodes the **identical
  40 scan positions and every raw level**; its raster block is the exact
  transpose of FFmpeg's, which cancels with the opposite IDCT pass order.
- The committed path passed a hardcoded `false` ("non-MBAFF: always frame
  mode") so the field scan was never used; that produced only 10 coefficients.
- The scan/dequant transposes are intentional and self-consistent. **No table
  change is justified.**

Fix: pass `hdr.field_pic_flag` to `decode_mb_cabac` in the non-MBAFF CABAC
slice loop. Verified at that commit: JVT FRext remains 42/69 with no regressions.

At that time full PAFF still needed field pairing, field MB addressing,
field-view MC and field-aware deblocking; this only made the entropy decode
correct. Appendix Q later landed a bounded step for those pieces.

---

## Appendix I: Field-view MC ground truth (2026-11-27)

Measured (`/tmp/ai-agent/PAFF_MC_PLAN.md`) on bottom P field MB(0,1) of
`Freh7_B.264`, n=0 in gdb:
- Reference = top field (`reference=1`), `data[0]` = frame base, stride **704**.
- Destination = frame base + **352** (one original linesize), MV `[0,-5]`,
  offset -1408, `emu=1`; prediction `[0,33,214,255]` plus all partitions/chroma.
- `pic_as_field`: bottom parity shifts each plane by one original frame
  linesize (352 luma / 176 chroma) then doubles strides (352->704, 176->352);
  top parity only doubles strides. Ref parent == current picture parent.
- Wedeo mapping: `mb.rs` MC call sites need a field-view branch
  (`&rp.y[parity*stride..]`, doubled stride, field-coordinate ref_y);
  `decoder.rs` needs field MB rows plus `fdc.mb_field=true`.
- **Deadlock rule:** skip `wait_for_row` when
  `Arc::ptr_eq(ref_pic.pic, ctx.pic.shared()) && ref_pic.is_field()`, and derive
  needed rows as `2*field_ref_bottom + parity`.

## Appendix J: Field-aware deblocking ground truth (2026-11-27)

Measured (`/tmp/ai-agent/PAFF_DEBLOCK_PLAN.md`) on the same vector:
- Bottom MB(0,1): `dest_y - data0 = 352`, `dest_cb - data1 = 176`,
  deblock stride 704/352, `block_offset` index 48 (reconstruction only).
- Its top horizontal edge is NOT filtered (`top_type=0`); first filtered edge
  at `pix=3168`, `bS=[2,2,2,2]`, `tc=[1,1,1,1]`, alpha=20, beta=7, stepping
  4 rows by 704 (frame rows 9,11,13,15).
- Bottom MB(0,3) top edge runs; above MB = bottom field MB(0,1).
- Wedeo's `deblock_luma_offset` / `deblock_chroma_offset` / tables already match
  numerically. Required: `mb_field = hdr.field_pic_flag` for pure PAFF, pass
  frame MB rows into `inline_deblock_row`/`deblock_last_row`, and never route
  PAFF through MBAFF pair deblocking. Shared `row_progress` means the top field
  must be published complete at pair start to avoid self-deadlock.

---

## Appendix K: Full PAFF attempt 2026-11-27 (reverted)

A complete attempt was made on top of `f4baf65` and reverted because it
introduced 4 new 120 s timeouts in the FRext suite (total errors 1 -> 5:
`brcm_freh6`, `Hi422FREXT16_SONY_A`, `Hi422FREXT18_SONY_A`,
`HPCVFL_BRCM_A`, `HPCVFLNL_BRCM_A`). Observed before revert:

- Field pairing + field MB addressing + field CABAC scan (the last already
  committed) made the **top field of `Freh7_B.264` bit-exact** with FFmpeg on
  every even row. This confirms Appendices E/H are correct.
- DPB placeholders are created with `RefStatus::Unused`, so a bottom field
  arriving before its top field's frame completes cannot see the top field as
  a reference. Promoting the paired entry to `ShortTerm` with `ref_mask=1`
  (top only) fixed self-reference (bottom rows changed from all-128 to real
  content). Setting `ref_mask=3` lets the bottom field self-reference its own
  unwritten rows and must be avoided.
- With `ref_mask=1`, decoded bottom-field MVs still differed from FFmpeg
  ground truth (`[0,-5]` expected; Wedeo produced `[0,1]`, `[4,1]`, ...), so
  the bottom-field CABAC context / MV prediction needs its own ground truth.
- Field-view MC (`&rp.y[parity*stride..]`, doubled stride, field-relative
  coords, skip `wait_for_row` for self-reference) is likely the source of the
  new timeouts; the exact deadlock was not isolated.

Next attempt should: (1) isolate the timeout with a single-vector run before
broad testing, (2) extract FFmpeg's bottom-field MV/context values at the
first MB, and (3) promote the DPB entry to ShortTerm with top-only mask as
above. Do not reapply this patch wholesale.

---

## Appendix L: Bottom-field CABAC ground truth (2026-11-27)

Extracted with gdb from the debug FFmpeg build. This supersedes the
MV-prediction hypothesis in Appendix K: prediction, ref selection and
probability states all agree; the arithmetic coder diverges earlier.

Measured on 2026-11-27. All values below are extracted directly with gdb from
`/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g` (debug build, `--disable-asm
--disable-optimizations --enable-debug=3`) running with
`-threads 1 -bitexact -i .../FRext/Freh7_B.264`.

## Vector and slice under test

The first bottom-field slice of `Freh7_B.264` is the complementary bottom field
of the IDR picture: `frame_num = 0`, `picture_structure = PICT_BOTTOM_FIELD (2)`,
`slice_type_nos = P` (2), `mb_field_decoding_flag = 1`, `mb_x/mb_y = 0/1` for the
first MB in field addressing (`mb_xy = 23`, `mb_stride = 23`, 22 MBs + padding).
The first coded 16x16 MB of that slice has final L0 MV `[0, -5]`.

## FFmpeg ground truth (gdb)

### Reference list at `h264_slice.c:1981` (after `ff_h264_build_ref_list`)

```
hit=1 frame_num=0 pic_struct=2 list_count=1
poc_lsb=0 delta_pic_order_cnt_bottom=0
curpic=0x555557cf3f80 curpic_frame_num=0 curpic_poc=65536
field_poc=[65536,65536] reference=1 first_field=0
short_ref_count=1
  short[0] frame_num=0 poc=65536 field_poc=[65536,65536] reference=1 long=0 pic_id=0
  ref_count[0]=1 curr_pic_num=1 max_pic_num=131072
  ref_list[0][0]: reference=1 (TOP field), poc=65536, pic_id=0,
                  parent=curpic, parent_frame_num=0, parent_reference=1,
                  data0=0x555557d23820, linesize0=704
```

There is exactly ONE L0 entry: the top field of the current picture
(`reference == PICT_TOP_FIELD`, i.e. field mask 1 / `ref_mask=1`). Its field POC
is 65536 (frame POC 0 in 1/2-pel units) and its `pic_id` is 0. The bottom field
is NOT in the list. This confirms the "promote only the top field" rule from
Appendix K; it also confirms `curr_pic_num = 1` for a field slice.

### `fill_decode_caches` at `h264_cabac.c:2069`, bottom MB(0,1)

```
mb_xy=23 mb_stride=23 mb_field=1 mb_type=12480
top_xy=-23 topleft_xy=-24 topright_xy=-22 left_xy=[22,22]
neighbor mb_types: top=0 topleft=0 topright=0 left_top=0 left_bot=0
intra4x4_pred_mode_cache[0..15] = 0 0 0 0 2 2 1 1 0 0 0 0 1 1 1 1
non_zero_count_cache[0..15]     = 3 0 0 0 0 0 0 0 0 0 0 0 3 3 3 3
top_cbp=15 left_cbp=15
```

Key details:
- `top_xy = mb_xy - (mb_stride << MB_FIELD) = 23 - 46 = -23`, so the top
  neighbor is outside the field, as expected for the first bottom-field row.
- `left_xy = mb_xy - 1 = 22`, which is the LAST MB of the top field row. All
  neighbor MB types are nevertheless zeroed by the `slice_table` check in
  `fill_decode_neighbors`: the top and bottom fields are different slices.
- The non-zero-count cache is NOT all zero: indexes 0 and 12..15 are 3 for an
  inter coded MB with unavailable neighbors (`nnz_excluded` semantics), which
  matters for CABAC CBP contexts.

### Final MV cache at `h264_cabac.c:2327` (after `write_back_motion`)

```
mb_type=12480 cbp_table=0, cached mb_type=0, ref_count=[1,1]
L0 mv_cache[ 0]=[0,0]     ref_cache[ 0]=0    mvd_cache[ 0]=[0,0]
L0 mv_cache[ 3]=[0,0]     ref_cache[ 3]=-2   mvd_cache[ 3]=[0,0]
L0 mv_cache[ 6]=[0,0]     ref_cache[ 6]=-2   mvd_cache[ 6]=[0,0]
L0 mv_cache[ 9]=[0,0]     ref_cache[ 9]=0    mvd_cache[ 9]=0
L0 mv_cache[12]=[0,-5]    ref_cache[12]=0    mvd_cache[12]=[0,5]
L0 mv_cache[15]=[0,0]     ref_cache[15]=0    mvd_cache[15]=[0,5]
```

`scan8[0] = 12`. The final MV `[0,-5]` comes from a zero MV predictor (all
three neighbors have `PART_NOT_AVAILABLE = -2`) plus a CABAC mvd with magnitude
5 and negative sign. The mvd magnitude is stored as `[0,5]` in `mvd_cache`, and
`get_cabac_bypass_sign(cabac, -mvd)` supplies the minus sign. The mvd context is
derived from `mvd_cache[scan8[0]-1] + mvd_cache[scan8[0]-8]`, both zero because
the cross-field neighbors are unavailable.

### MV predictor and mvd context at `h264_cabac.c:2178` (inside P_8x8 decode)

```
mb_type=12480 (0x30c0 = MB_TYPE_8x8 | MB_TYPE_INTERLACED | P0L0 | P1L0)
predicted mx=0 my=0 (before mvd), scan8[0]=12
L0 ref_cache = [0,0,0,-2,-2,-2,-2,-2,0,0,-2,0,0,-2,0,0]
L0 mv_cache  = all [0,0]
L0 mvd_cache = all [0,0]
neighbor mvd sums: amvd0=0 amvd1=0
CABAC state bytes [40]=0 [47]=10 (before mvd decode)
```

So the MB is P_8x8 (not 16x16); the first sub-MB MV comes from the
`pred_motion` call at `h264_cabac.c:2177` with all three neighbors
`PART_NOT_AVAILABLE`, hence predictor `[0,0]`. The mvd context indices are 40
and 47 (both neighbor sums 0). This is the exact input state that Wedeo must
reproduce for the first bottom-field MB.

## Wedeo ground truth (baseline HEAD `f6d844c`, temporary `trace!` probes, reverted)

The baseline decoder does not pair the fields, so the bottom slice is decoded
as a separate picture with `mb_x/mb_y = 0/0`. This is still the same first
coded MB of the same bitstream slice, so the CABAC inputs are comparable.
`wedeo-framecrc` with `RUST_LOG=wedeo_codec_h264::mb=trace,
wedeo_codec_h264::cabac=trace`:

```
decoded MB mb_x=0 mb_y=0 qp=28 mb_type=3 cbp=7
P_8x8 sub i8x8=0 sub_type=1 ref_idx=0
P_8x4 MV sub=0: mv_a=[0,0] ref_a=-1 avail=false
                mv_b=[0,0] ref_b=-1 avail=false
                mv_c=[0,0] ref_c=-1 avail=false
                mvp=[0,0] mvd=[0,1] mv=[0,1]
CABAC probe at first mvd: amvd0=0 amvd1=0
                          state[40]=0 state[47]=10
                          state[0]=86 state[1]=12 state[2]=31
                          state[3]=86 state[4]=12 state[11]=15 state[16]=16
CABAC engine at first mvd: low=17072128 range=394 pos=4
```

This reproduces the `[0,1]` MV quoted in Appendix K. The later MVs (`[4,1]`,
`[0,11]`, ...) are downstream consequences of the same first mvd difference.

## Exact first divergence and conclusion

Aligned side by side at the first mvd bin of the bottom-field first MB:

| Quantity | FFmpeg | Wedeo |
|---|---|---|
| ref list L0 | 1 entry, top field only, `poc=65536`, `pic_id=0`, `frame_num=0` | 1 entry (baseline: parity unset; Appendix K attempt: top only) |
| predictor | `[0,0]`, all neighbors unavailable | `[0,0]`, all neighbors unavailable |
| `amvd0/amvd1` | `0/0` | `0/0` |
| `state[40]/state[47]` | `0/10` | `0/10` |
| state bytes `0,1,2,3,4,11,16` | `86,12,31,86,12,15,16` | `86,12,31,86,12,15,16` |
| sub-MB type 0 | code 1 (`16x8`/field `8x4`) | code 1 (`8x4`) |
| CABAC engine `low/range/pos` | `16846600 / 271 / 4` | `17072128 / 394 / 4` |
| decoded mvd | `[0,-5]` | `[0,1]` |

The MV prediction, reference POC/parity selection and the CABAC *state array*
are NOT the first divergence: they agree exactly. The first measured divergence
is the CABAC arithmetic engine (`low`/`range`) when entering the mvd syntax
element, even though both decoders are at byte position 4 and have identical
probability states. That means the two decoders consumed a different bin
sequence (a different number of bins in the MB-type/sub-MB-type path) before
the mvd, so the bitstream cursor inside the arithmetic coder is different.
This is an entropy-stage bug, not an MC/deblock/DPB bug.

No single-cause fix was measured, so NO patch was applied and no commit was
made. The worktree remains clean at `f6d844c`. The next measurement should
capture the per-bin trace from CABAC slice start to the first mvd in both
decoders (bin index, context index, bin value, `low`, `range`) and find the
first bin where `low`/`range` or the context index diverges. Likely suspects
are the sub_mb_type context derivation (FFmpeg `decode_cabac_p_mb_sub_type`
uses neighbor sub-partition state) and the `mb_skip_flag`/`mb_type` bin order
for field slices, but this is a hypothesis only and must not be patched
without a measured bin-level divergence.

---

## Appendix M: MBAFF first divergence (HCAMFF1_HHI, 2026-11-27)

# H.264 MBAFF measured ground truth: HCAMFF1_HHI (2026-11-27)

Agent worktree: `/home/hakergeniusz/.local/share/opencode/worktree/695063/h264-mbaff`
Base: HEAD `5e47eb4`, tree left clean. No patch committed.

## Vector and reproduction

Chosen vector: `/var/cache/zstd5/wedeo-ref/jvt-vectors/JVT-FR-EXT/HCAMFF1_HHI.264`
(19,978 bytes, 352x288, yuv420p, 10 frames). Per `PAFF_RESEARCH.md` Appendix B
this is MBAFF-only (0 top / 0 bottom / 10 frame slices), so the PAFF pairing path
is not involved. It is not `Freh7_B.264` (owned by another agent) and is not one
of the vectors that previously hit 120 s timeouts.

Commands (both decoders, raw YUV byte comparison):

```
wedeo-framecrc <vec> --raw-yuv <wedeo.yuv>
/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g -y -v error -threads 1 -bitexact \
  -i <vec> -f rawvideo -pix_fmt yuv420p <ffmpeg.yuv>
python3 /tmp/ai-agent/mbaff_compare.py <vec>
```

Reproduction is deterministic: the prebuilt
`/var/cache/zstd5/wedeo-target/release/wedeo-framecrc` and a fresh release build
from this worktree produced identical first-divergence values.

Baseline (deblocking on): all 10 frames DIFF; frame 0 first divergence at
Y x=95, y=0, Wedeo 85 vs FFmpeg 84; 80,609 differing Y pixels, 11,830 U,
11,824 V; max abs delta 255.

With `-skip_loop_filter all` / `WEDEO_NO_DEBLOCK=1`: frame 0 first divergence
at Y x=96, y=0, Wedeo 81 vs FFmpeg 80; 79,944 differing Y pixels. The divergence
is therefore in reconstruction, not deblocking.

First-frame difference map (16x16 MB grid, no deblock), rows 0-5:

```
   0    0    0    2  128  128  232  256  256  256  256  256  256  256  256  256  256  256  256  256  256  256
   0    0    0    0  128  128  128  216  256  256  256  256  256  256  256  256  256  256  256  256  256  256
   0    0    0    0  128  171  256  208  256  256  256  256  256  256  256  256  256  256  256  256  256  256
   0    0    0    0    0   64  256  240  256  255  256  256  256  256  256  256  256  256  256  256  256  256
   0    0    0    0    0   64  255  256  256  256  256  256  256  256  256  256  256  256  256  256  256  256
   0    0    0    0    0  156  256  256  256  256  256  256  256  256  256  256  256  256  256  256  256  256
```

The first field-mode pair is MB x=3 (MB addresses 3 and 26, mb_stride=22).
Pixels of MB x=5,6 and onward diverge by a constant +1/255 pattern on many
rows, consistent with an intra prediction / coefficient placement divergence
at the field/frame transition (MB x=3 field, MB x=6 frame).

## FFmpeg ground truth (gdb, debug build)

Debug binary: `/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g`
(`--disable-optimizations --enable-debug=3 --disable-stripping --disable-asm`),
`-threads 1 -bitexact`. GDB scripts are in `/tmp/ai-agent/mbaff_*.gdb`, raw output
in `/tmp/ai-agent/mbaff_ffmpeg_*.txt`.

### Field flags (`h264_cabac.c:1966`, 2859 records)

Frame 0, `pict_type=1` (I): pairs x=0,1,2 are frame mode; x=3,4,5 are field mode
(`mb_field_decoding_flag=1`, `mb_mbaff=1`); x=6 is frame mode; x=7 onward is
field mode. Wedeo's `MBAFF_MB_START` trace for frame 0 shows the same
`mb_field=true/false` sequence, so the field flags themselves agree.

### Field-mode MB(3,1), mb_xy=26 (`h264_cabac.c:2069` / `:2090` / `:2341`)

```
mb_type=16777345 (INTRA4x4 | INTERLACED), QP=34, cbp=0x1f,
left_cbp=0xa, top_cbp=0x7cf,
intra4x4 mode cache (scan8 order) = 8 8 8 8 2 2 2 2 2 2 2 2 2 2 2 2,
nnz cache = 5 5 5 5 7 7 7 7 2 2 2 2 3 3 3 3,
decoded coefficients (raster, first 16): 384 150 0 -240 0 0 0 0 -150 0 0 0 0 0 0 0,
chroma DC: -1 0 0 0 0 0 0 0.
```

FFmpeg intra mode decode at `h264_cabac.c:2077` (before
`ff_h264_check_intra4x4_pred_mode`) for `sl->mb_xy == 26`:

```
pred=2 for every 8x8 group; states s68=2/4/2/0, s69=3/9/9/9, s399=15;
cabac low/range = 4354816/358, 34838528/281, 29569024/256, 25583616/256;
decoded modes = 1, 9, 1, 4 (then write_back/check).
```

The reconstruction-time 8x8 modes at `h264_mb.c:651` are `8, 9, 2, 2` with
nnz=4 for each 8x8 block; first 8x8 prediction samples `p0=100, p1=95`.

### Field-mode MB(3,0), mb_xy=3

```
mb_type=16777345, QP=33, cbp=0x1f, left_cbp=0xa, top_cbp=0x7cf,
coefficients: 192 150 0... ; chroma DC 1 0 ...
stored intra4x4 modes (mb2br layout) = 1 1 4 4 4 2 2 0
```

### QP progression frame 0 (gdb `h264_cabac.c:2491`)

QP increments by 1 for each of the first 8 MBs, reaching 34 at MB(3,1);
Wedeo's `decoded MB` trace shows the same QP sequence.

### FFmpeg field scan tables (`gdb print h->field_scan8x8`)

`h->field_scan[0..16]` = `0 1 4 2 3 5 6 7 8 9 10 11 12 13 14 15`
`h->field_scan8x8[0..16]` = `0 1 2 8 9 3 4 10 16 11 5 6 7 12 17 24`

These are the *transposed* variants from `init_scan_tables`
(`h264_slice.c:764-795`); the raw `field_scan`/`field_scan8x8` constants in
`h264_slice.c:53-78` are different. CABAC residual uses the transposed variants
(`h264_cabac.c:2424-2432`).

## Wedeo traces (matching equivalent points)

Wedeo run with `RUST_LOG=wedeo_codec_h264=trace`, copied to
`/tmp/ai-agent/mbaff-work/wedeo-framecrc-trace`.

Field flags at `MBAFF_MB_START` agree with FFmpeg. At MB(3,1):

```
INTRA_MODE_CTX mb_x=3 mb_y=1 scan_idx=0 left_mode=2 top_mode=-1 predicted=2 mode=8
INTRA_MODE_CTX mb_x=3 mb_y=1 scan_idx=4 left_mode=8 top_mode=-1 predicted=2 mode=2
INTRA_MODE_CTX mb_x=3 mb_y=1 scan_idx=8 left_mode=2 top_mode=8 predicted=2 mode=2
INTRA_MODE_CTX mb_x=3 mb_y=1 scan_idx=12 left_mode=2 top_mode=2 predicted=2 mode=2
```

FFmpeg decoded `1, 9, 1, 4` from the same `pred=2`; Wedeo decoded `8, 2, 2, 2`.
Both decoders agree on the predictor input (`pred=2`), but Wedeo's decoded
CABAC bins differ at the first 8x8 group. The most likely root cause is an
earlier divergence in CABAC state, most plausibly in MB(3,0) residual decoding,
because the decoded coefficient values for that field MB also differ
(FFmpeg `[192,150,0,...]` vs Wedeo `DEQUANT coeff_sum=924 dc=384` for i8x8=0).

Caveat: FFmpeg's `intra4x4_pred_mode_cache` layout (scan8) and Wedeo's
`mode_cache` (raster) are different, so the raw cached mode arrays are not
directly comparable. The `pred=2` values above are from FFmpeg's
`pred_intra_mode` return value and Wedeo's `predicted` field respectively,
which are directly comparable.

## Hypotheses tested and rejected (measured)

1. Left-block intra mode remap for field-mode bottom MB
   (`compute_mbaff_neighbors` option 3 / `LEFT_LUMA_NZ_ROW`). Checked against
   FFmpeg `left_block_options[3]` and `pred_intra_mode`; both decoders use
   `pred=2` for the first 8x8 group, so the predictor is not the cause.
2. Missing transpose of the field scan tables in Wedeo's CABAC residual:
   changed `FIELD_SCAN_4X4` to `[0,1,4,2,3,5,6,7,8,9,10,11,12,13,14,15]` and
   `FIELD_SCAN_8X8_USIZE` to the transposed `tables::FIELD_SCAN_8X8`, matching
   FFmpeg `init_scan_tables`. Built and measured on HCAMFF1_HHI: first diff
   moved *earlier* (Y x=47,y=0 vs x=95,y=0) and the differing Y pixel count
   grew from 80,609 to 97,457. Reverted.

The transposed-scan hypothesis is rejected: it changes the bin stream and
makes the vector worse, so it is not a valid single-cause fix.

## Next step recommendation

Extract Wedeo's `CabacReader::low`, `range` and `state[68]/[69]/[399]` immediately
before `decode_cabac_mb_intra4x4_pred_mode` at MB(3,1) i=0, and compare with
FFmpeg's `4354816/358, s68=2, s69=3, s399=15`. Then walk one MB back to
MB(3,0) residual decoding and compare the first CABAC bin at which `low`/`range`
diverge, following the `h264_cabac.c:2077` - `h264_mvpred.h:42` -
`h264_cabac.c:2424` chain. Do not apply hypothesis-only patches: both tried
hypotheses were rejected by measurement.

## Files produced

- `/tmp/ai-agent/mbaff_compare.py`, `mbaff_diffmap.py` (raw YUV comparison)
- `/tmp/ai-agent/mbaff_work/HCAMFF1_HHI.{wedeo,ffmpeg}.yuv`
- `/tmp/ai-agent/mbaff_wedeo_trace.log`, `mbaff_wedeo_trace2.log`
- `/tmp/ai-agent/mbaff_ffmpeg_flags.txt`, `mbaff_ffmpeg_neighbors.txt`,
  `mbaff_ffmpeg_mb6.txt`, `mbaff_ffmpeg_mb6b.txt`, `mbaff_ffmpeg_qp.txt`,
  `mbaff_ffmpeg_mv.txt`, `mbaff_ffmpeg_mb6*.txt`
- `/tmp/ai-agent/mbaff_*.gdb` (gdb scripts)
- `/tmp/ai-agent/mbaff_compare_baseline.txt`, `mbaff_compare_nodeblock.txt`,
  `mbaff_compare_scanfix.txt`
- `/tmp/ai-agent/mbaff_build*.log`

---

## Appendix N: First divergent PAFF CABAC bin (2026-11-27)

Fixed at commit `e2ccefa`: Wedeo used `mb_field` where FFmpeg uses
`mb_mbaff` for `rc = ref_count << MB_MBAFF(sl)`. Pure PAFF field pictures
have `mb_field=1` but `mb_mbaff=0`, so Wedeo decoded 10 extra ref_idx bins
before mvd. After the fix, bins 0-9 and the first mvd ([0,-5]) match FFmpeg
exactly with no suite regression (42/69 FRext at that commit, zero new
timeouts). Final PAFF pixels still diverged because full field pairing,
field-view MC and field deblock were out of scope for this entropy fix;
Appendix Q later added a bounded implementation.

# H.264 PAFF bottom-field first divergent CABAC bin (2026-11-27)

Worktree: `/home/hakergeniusz/.local/share/opencode/worktree/695063/paff-bins`
Base: HEAD `5e47eb4`. Temporary `tracing::trace!` probes were added and then
fully reverted; no probe code remains in the final diff.

## Vector and slice

`/var/cache/zstd5/wedeo-ref/jvt-vectors/JVT-FR-EXT/Freh7_B.264`, first
bottom-field P slice (`frame_num=0`, `picture_structure=PICT_BOTTOM_FIELD`,
`mb_field_decoding_flag=1`, `mb_x=0/mb_y=1`, first coded MB is P_8x8 with
sub-MB type 1 / 8x4). This is the complementary bottom field of the IDR picture.

## Method

FFmpeg: gdb on `/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g`
(`--disable-asm --disable-optimizations --enable-debug=3`), breakpoints at
`ff_h264_decode_mb_cabac`, `libavcodec/cabac_functions.h:128` (post-bin in the
inlined `get_cabac`), `:150`/`:153` (bypass), plus marker breakpoints at
`h264_cabac.c:1938` (mb_skip_flag), `:2004` (P mb_type), `:2130` (sub_mb_type)
and `:2178` (first mvd). GDB scripts: `/tmp/ai-agent/ffmpeg_trace.gdb`,
`ffmpeg_trace2.gdb`; raw output `ffmpeg_trace2.out`.

Wedeo: temporary `trace!` probes inside `CabacReader::get_cabac`,
`get_cabac_bypass`, `get_cabac_bypass_sign`, `get_cabac_terminate`, plus markers
at P mb_type, sub_mb_type and mvd entry in `cabac.rs` and a bottom-field slice
gate in `decoder.rs`. Built with `RUST_LOG=wedeo_codec_h264::cabac=trace`.
Trace output: `/tmp/ai-agent/wedeo_trace_full.log` (baseline) and
`wedeo_trace_fixed.log` (after fix). Slice start and P-sub-MB-type states
matched FFmpeg in both.

## Measured per-bin trace, slice start to first mvd

Bins are numbered from 0 at the mb_skip_flag of the first bottom MB. `ctx` is
the CABAC context-state index. Values are post-bin `low/range/pos`.

| # | ctx | bin | FFmpeg low/range/pos | Wedeo low/range/pos | match |
|---|-----|-----|----------------------|---------------------|-------|
| 0 | 11 | 0 | 28898304/300/2 | 28898304/300/2 | yes |
| 1 | 14 | 0 | 28898304/291/2 | 28898304/291/2 | yes |
| 2 | 15 | 0 | 57796608/444/2 | 57796608/444/2 | yes |
| 3 | 16 | 1 | 62889984/492/2 | 62889984/492/2 | yes |
| 4 | 21 | 0 | 42680320/350/2 | 42680320/350/2 | yes |
| 5 | 22 | 0 | 35028992/316/2 | 35028992/316/2 | yes |
| 6 | 21 | 0 | 35275714/464/4 | 35275714/464/4 | yes |
| 7 | 22 | 0 | 8423300/454/4 | 8423300/454/4 | yes |
| 8 | 21 | 1 | 16846600/476/4 | 16846600/476/4 | yes |
| 9 | 21 | 1 | 16846600/271/4 | 16846600/271/4 | yes |

At bin 9 (the last sub_mb_type bin), both decoders are at exactly
`low=16846600 range=271 pos=4`, matching Appendix L's FFmpeg value. This is
the first mvd entry state in FFmpeg.

### First divergence

| # | ctx | bin | FFmpeg | Wedeo baseline |
|---|-----|-----|--------|----------------|
| 10 | 40 | 0 | `33693200/286/4` (mvd_x, state[40]=0) | `33693200/362/4` (ctx=54 ref_idx!) |

Wedeo consumed an extra `ref_idx` bin at context 54 before mvd. The extra bins
were `ctx=54 bin=0`, `ctx=54 bin=0`, `ctx=54 bin=1`, `ctx=58`, `ctx=59 x5`,
`ctx=55` (bins 10 through 20), after which Wedeo entered mvd at
`low=17072128 range=394 pos=4` and decoded `[0,1]` instead of FFmpeg's
`[0,-5]`. This exactly reproduces Appendix L's baseline `low/range`.

## Root cause (measured, not hypothesised)

FFmpeg `h264_cabac.c:2140` decodes ref_idx only when
`rc = sl->ref_count[list] << MB_MBAFF(sl)` is greater than 1.
`MB_MBAFF(h)` is `h->mb_mbaff = h->mb_aff_frame && mb_field_decoding_flag`,
which is 0 for pure PAFF field pictures. Wedeo's `decode_mb_cabac` used

```
let ref_count_l0 = num_ref_idx_l0_active << u32::from(mb_field);
```

`mb_field` was passed `hdr.field_pic_flag` (true for PAFF), so the single
frame reference was doubled to 2 and every P_8x8 partition decoded a spurious
ref_idx bin. The FFmpeg debug build confirms `mb_xy=389, mb_x=0, mb_y=1,
mb_field=1`, `ref_count[0]=1`, and no ref_idx bin in the trace.

## Minimal fix

Add an explicit `mb_mbaff` parameter to `decode_mb_cabac` and use it for the
doubling. The non-MBAFF PAFF path passes `false`; the MBAFF pair path passes
`true`. No ref-list, MC, deblock or DPB code was touched.

After the fix, the Wedeo per-bin trace is byte-identical to FFmpeg through the
first mvd entry (`WDMARK mvd_start amvd0=0 amvd1=0 states[40,47]=[0,10]
low=16846600 range=271 pos=4`); the first mvd bins then also match FFmpeg's
trace (mvd_x `ctx=40 bin=0`, `low=33693200 range=286` after renormalization).

## Verification status (final)

- Per-bin parity after fix: Wedeo bins 0-9 and the first mvd entry now match
  FFmpeg byte-for-byte (`low=16846600 range=271 pos=4`; first mvd bins
  `ctx=40 bin=0`, `low=33693200 range=286`). The spurious ctx=54 ref_idx bins
  are gone.
- File-level framecrc: `wedeo-framecrc Freh7_B.264` produces 205 lines both
  before and after; field CRCs from the first bottom-field slice onward change.
- Pixel comparison against FFmpeg raw YUV: the bottom-field luma rows do NOT
  reach parity (about 50680 differing luma bytes per frame before and after).
  The decoded MVs now match FFmpeg at the first MB, but final pixels stay wrong
  because full PAFF reconstruction (field pairing, field-view MC, field
  deblock) was unimplemented at that commit and explicitly out of scope for
  this task; Appendix Q later added a bounded step.
- Debug-build panic at `mb.rs:3368` (`attempt to subtract with overflow`) was
  seen in one slow debug trace configuration; release builds decode all frames
  without it. Not part of this entropy fix.
- Full JVT FRext suite: baseline 42/69 and fixed 42/69, identical 27-name FAIL
  set (`diff` of sorted FAIL lists is empty), zero `timed out`/`ERROR` lines.
  Run with `JVT_VECTORS=/var/cache/zstd5/wedeo-ref/jvt-vectors python3
  scripts/suite_runner.py --suite jvt-fr-ext` on release binaries.
- `cargo fmt --all -- --check` and `cargo clippy -p wedeo-codec-h264
  --all-targets` both clean (only pre-existing workspace-dependency warnings).

## Conclusion

The first divergent CABAC bin was measured: bin 10, context 54 (ref_idx) in
Wedeo, where FFmpeg goes directly to mvd context 40. One measured root cause:
PAFF incorrectly doubled `ref_count` because `mb_field` was used in place of
`mb_mbaff`. The minimal fix restores full bin-level parity through the first
mvd and kept the suite at 42/69 at that commit with no regressions or timeouts.
Final bottom row pixel parity remained blocked by full PAFF reconstruction,
which this task explicitly excluded; Appendix Q later added a bounded step.
Committed as `ec3409b`,
worktree clean, not pushed.

---

## Appendix O: PAFF deblock ground truth (2026-11-27)

Measured on Freh7_B.264 first bottom-field MB with gdb on the debug FFmpeg
build. Conclusion: `deblock.rs` needs no change; the required field addressing
must come from decoder.rs / mb.rs.

| Quantity | Measured FFmpeg value |
|---|---|
| mb_xy / mb_x / mb_y | 23 / 0 / 1 |
| picture_structure / mb_aff_frame / MB_FIELD / MB_MBAFF | 2 / 0 / 1 / 0 |
| deblocking_filter | 1 (neighbours blocked only by slice_table == 0xFFFF) |
| top_xy | 23 - (23 << 1) = -23, slice_table 0xFFFF, top_type 0 |
| left_xy | 22 (end-of-row padding), slice_table 0xFFFF, left_type 0 |
| dest_y / dest_cb offsets | 352 (= 5632 - 15*352) / 176 |
| deblock stride | 704 / 352 (doubled by MB_FIELD) |
| filter path | `ff_h264_filter_mb_fast`, not the MBAFF `ff_h264_filter_mb` |
| edge at pix=3168 | bS=[2,2,2,2], tc=[1,1,1,1], alpha=20, beta=7, stride=704 |
| mb_y=3 | dest_y offset 11616 (pixel row 33), top_xy=23, slice_table[23]=1 (top neighbour is bottom-field MB(0,1)) |

Required edges:

- Bottom first row (mb_y=1): skip top horizontal edge (top_xy=-23) and left
  vertical edge at mb_x=0. For mb_x>0 the left neighbour is mb_xy-1 in the
  same bottom-field row.
- Bottom rows mb_y=3,5,...: top neighbour is `mb_xy - 2*mb_stride`, not
  `mb_xy - mb_stride` (which is the top field).
- All edges use doubled stride and a bottom-field start offset. Wedeo's
  `deblock_luma_offset` / `deblock_chroma_offset` formulas already match
  FFmpeg when `mb_field=true` and a FRAME MB row is passed.
- mvy_limit=2 and intra top-edge bS=3 are field-flag driven and already
  covered by Wedeo's `cur.mb_field` branch.
- Pure PAFF must not enter the MBAFF first-vert / horiz-edge0 special cases;
  marking all PAFF MBs as field makes those inert.

## Appendix P: deblock measured state in current Wedeo (2026-11-27)

Existing `DEBLOCK_MB_START` / `DEBLOCK_EDGE` traces show the top field starts
at mb_x=0/mb_y=0, then the bottom slice ALSO starts at mb_x=0/mb_y=0, with
`cur_mb_field=false` and `y_stride=352` in both. bS is 2 as in FFmpeg but is
applied to the wrong rows with the wrong stride.

Raw YUV at that time: FFmpeg emits 150 frames, Wedeo 300 (before Appendix Q
field pairing). Comparing
frame 0, the first 16 rows have 2344/2816 differing even rows and 2718/2816
differing odd rows. With `WEDEO_NO_DEBLOCK=1`, the first 32 rows still have
5154/5632 differing even rows, so reconstruction fails before deblocking.
A deblock-only patch cannot be validated on this vector.

Actionable requirements for the decoder.rs / mb.rs owner:
1. Pure PAFF: `mb_field = hdr.field_pic_flag`.
2. Pass FRAME MB rows to deblock (`mb_y = 2*field_y + parity`), not field rows.
3. Do not call `deblock_row_mbaff` for pure PAFF.
4. Bottom-field top neighbour is `mb_xy - 2*mb_stride`; stride is doubled.

Evidence: /tmp/ai-agent/PAFF_DEBLOCK_TASK_NOTES.md and gdb scripts
/tmp/ai-agent/paff_deblock_ff{,2,3,4}.gdb (ephemeral).

---

## Appendix Q: PAFF field pairing + field-view MC (commit e61b4af, 2026-11-27)

Implemented as a measured, bounded step toward full PAFF. Bottom-field slices
now join the top field's `FrameDecodeContext` instead of starting a second
frame. Field slices map `first_mb_in_slice` to frame MB rows, bottom MBs are
marked `mb_field=true`, MBAFF pair deblock is skipped for pure PAFF, and the
paired DPB entry is promoted to `ShortTerm` with `ref_mask=1` (top only).
Field references in MC use a parity offset plus doubled stride and skip
`wait_for_row` for same-picture top-field references.

Deadlock isolation: the first single-vector 30 s run had no hang; the full
FRext run exposed three 120 s timeouts. These were removed by keeping bottom
field deblock from regressing row progress, marking the paired picture complete,
and temporarily skipping high-depth reconstruction on bottom fields.

Measured on Freh7_B.264: 105 output rows (baseline 205, FFmpeg 106). First 16
luma rows: even-row differences 2344 -> 473, odd-row differences 2718 -> 2034.
Full FRext: 42/69 MATCH at that commit, identical FAIL set to baseline, 0 ERROR
and 0 timeouts (43/69 after the high-depth IDCT fix, 44/69 at HEAD).

Trade-offs originally retained for the next pass (the first two were later
superseded by Appendices R and S):
- Top-field first row is no longer byte-exact (about 50 +/-1 deblock differences)
  because deblock now runs on paired frame rows. Fixed at `056ffec` (Appendix R).
- High-depth bottom fields skip `post_decode_mb_hires`; those streams are not
  bit-exact and this pass does not make them worse in the suite. Removed at
  `9636f08` (Appendix S).
- `mark_complete()` on pairing is a deliberate guard against self-reference
  waits; a proper per-field row-progress model should replace it.

Do not reapply the older wholesale attempt of Appendix K; this patch is the
measured successor.

---

## Appendix R: top-field double-deblock fix (commit 056ffec, 2026-11-27)

gdb on Freh7_B.264 showed FFmpeg filters each field row exactly once (top
rows 0,2,4,..., then bottom rows 1,3,5,...). Wedeo tracing showed frame row 0
being filtered twice: the top slice's `inline_deblock_row` filtered it on
completing row 1, then the bottom slice's `inline_deblock_paff_bottom_row`
filtered `completed_row - 2 = 0` again. That produced about 50 +/-1 pixel
differences and was the sole cause of the top-field regression recorded in
Appendix Q.

Fix: pass the bottom slice's first frame row to
`inline_deblock_paff_bottom_row` and skip any target row above it. Bottom-field
row filtering is unchanged.

Verified: Freh7_B.264 row 0 byte-exact; even rows 0-15 have 0 differences
(was 473); odd rows 1-15 keep 2034 differences (the Appendix Q improvement).
FRext 43/69 MATCH, 0 errors, 0 timeouts.

---

## Appendix S: high-depth bottom-field reconstruction restored (commit 9636f08, 2026-11-27)

Removed the temporary `post_decode_mb_hires()` skip on PAFF bottom fields from
Appendix Q. `hires.rs` now uses `pic_as_field` semantics for field references:
doubled stride, one-linesize offset for the bottom field, field-relative MVs,
and a skipped `wait_for_row()` for same-picture field references (no
`mark_complete()` hack). u16 chroma destinations use the field stride too.

Verified no hang on `Hi422FREXT16` and `Hi422FREXT18` with 30 s timeouts.
`Hi422FREXT16` luma absolute error fell from 1.7353e9 to 1.6780e9 and differing
pixels from 5,218,985 to 5,145,856; U/V diffs also fell about 1.6%. Four-suite
baseline stays 198/283 with identical pass sets and no new timeouts or errors
(199/283 at HEAD after `b46b0fc`).
The vector is still not frame-exact; remaining error is from other stages.

---

## Appendix T: MBAFF CABAC exonerated at current HEAD (2026-11-27)

Measured on `HCAMFF1_HHI.264` at HEAD `8e13c1e`. The earlier Appendix M
divergence (FFmpeg sub-MB modes 1,9,1,4 vs Wedeo 8,2,2,2) was from the old
`5e47eb4` baseline and was resolved by subsequent CABAC fixes, including the
mb_mbaff ref-count fix. The pixel divergence remains: first luma diff x=95,
y=0 with 80,609 differing Y pixels.

gdb on FFmpeg `ff_h264_decode_mb_cabac` gated on MB(3,0) captured 1400
contextual CABAC bins; Wedeo temporary tracing recorded ctx, bin, low and
range for each. After alignment, 1399 consecutive contextual bins matched in
ctx, bin and range from MB(3,0) through MB(10,0), including MB(6,0) where the
pixel divergence starts. MB(3,1) intra4x4 bins also matched with identical
decoded modes.

Conclusion: CABAC entropy decoding and context derivation are no longer the
cause. The remaining divergence is downstream in reconstruction (coefficient
placement, prediction or field deblock). Do not repeat a CABAC-bin search on
this vector. Evidence: `/tmp/ai-agent/mbaff_paff_evidence.md` (ephemeral).

---

## Appendix U: PAFF bottom MB mvd divergence on HCAFF1_HHI (2026-11-27)

Vector `HCAFF1_HHI.264` (352x288, yuv420p, 10 frames, PAFF+frame mixed).
Even rows of frame 0 match byte-for-byte; odd rows differ by about 300 pixels
each (95023 Y diffs total), first at Y(14,1) FFmpeg 88 / Wedeo 89. With
deblocking disabled on both decoders the odd rows still differ by 94586
pixels and the first diff moves to (16,1) FFmpeg 92 / Wedeo 105, so deblocking
only propagates an upstream error.

First inconsistent MB is bottom MB(1,1). Syntax parses identically on both
sides: `mb_type=P_L0_16x16`, qp=31, cbp=0, ref_count[0]=1, mb_skip_flag=0,
reference is the top field (ref_mask=1, linesize=704, offset 0). Paired
ground truth:

| Quantity | FFmpeg | Wedeo |
|---|---|---|
| MB(0,1) mb_type / mvd | 12432 (16x8|L0) / [38,3], [1,0] | 1 (16x8) / [38,3], [1,0] |
| MB(1,1) MV predictor | [38,3] | [38,3] |
| MB(1,1) mvd | [1,0] | [1,-2] |
| MB(1,1) final MV | [39,3] | [39,1] |

The first measurable divergence is one CABAC `mvd_y` bin in the second bottom
MB. MB type, CBP, skip flag, reference selection and mvd prediction context
already agree. The wrong MV is then the input to MC, so reconstruction/MC/
deblock patches cannot fix it. This belongs to the CABAC workstream.

Evidence: `/tmp/ai-agent/paffrecon_evidence.md` (ephemeral).

---

## Appendix V: MBAFF 8x8 intra above-left neighbor fix (commit 1f5a1fb, 2026-11-27)

gdb on FFmpeg MB(4,1) of `HCAMFF1_HHI.264` measured 8x8 intra prediction sums
5632/6656/6244/6464; Wedeo's values were all 64 higher (+1 per pixel). The
cause was in `decode_intra8x8`: `has_above_left` used `mb_y - 1`, but in MBAFF
field mode the previous field row is two frame MB rows up (FFmpeg
`h264_mvpred.h:500`: `top_xy = mb_xy - (mb_stride << MB_FIELD)`). For a
bottom-field MB in the first pair (mb_y=1) no above-left sample exists, yet
Wedeo treated a fallback 128 as available, shifting `fl[0]` from 77 to 91 and
DC prediction from 88 to 89. The +1 propagated through left neighbors to
frame-mode MB(6,0), explaining the even-row exact / odd-row biased pattern.

Fix: when `ctx.is_mbaff && ctx.mb_field`, use `mb_y - 2` with availability
`mb_y >= 2`; otherwise keep `mb_y - 1`. Verified prediction sums now match
FFmpeg exactly. With deblock off, frame 0 first diff moved (96,0) -> (288,128)
and differing Y pixels fell 79944 -> 70275; with deblock on, (95,0) ->
(36,30) and 80609 -> 71193. FRext stays 44/69 with no new timeouts.

---

## Appendix W: MBAFF CABAC measurement pitfall (2026-11-27)

On `FREXT01_JVC_D.264` (MBAFF-only, 352x288, 16 frames) with deblocking off,
frame 0 first differs at Y(32,0): Wedeo 128 vs FFmpeg 255, 150141 differing
pixels. Comparing `cabac_state` via gdb: MB(0,0) and MB(1,0) entry differ only
in ctx70 (an artifact of different `mb_field_decoding_flag` decode timing),
while MB(1,1) has 33 differing states and MB(2,0) has 109. The true divergence
is localized at MB(1,0).

An attempted paired bin trace using an FFmpeg gdb breakpoint plus a Wedeo
out-of-line `get_cabac` breakpoint reported the first difference as FFmpeg
ctx96 vs Wedeo ctx134. That was a measurement artifact: Wedeo's `RUST_LOG`
shows MB(0,0) did decode the cat=2 block_idx=0 cbf bin (ctx96), and MB0/MB1
pixels are byte-identical. LLVM inlining made the Wedeo breakpoint miss bins.
Do not treat breakpoint-only bin traces as ground truth; use source-level
temporary `trace!()` probes in Wedeo to pair bins at MB(1,0).

No patch, no commit; the 18-minute cold build did not finish under parallel
load so the remaining gates were not run. Evidence:
`/tmp/ai-agent/mbaffcab_no_fix_evidence.md` (ephemeral).

---

## Appendix X: pure-PAFF CABAC now aligned (2026-11-28, no patch)

After commit eb59a58 (MAP_MVS gated on FRAME_MBAFF), gdb on `Freh7_B.264`
first bottom MB (mb_x=0, mb_y=1) and Wedeo source-level tracing agree exactly:
mvd `[0,-5]`, predictor `[0,0]`, final MV `[0,-5]`; second partition mvd `[0,9]`,
predictor `[0,-5]`, final MV `[0,4]`; mvd entry low=16846600 range=271 pos=4,
state40/47/48 = 0/10/13. This resolves the divergence first seen in
Appendix L/N. `HCAFF1_HHI` also matches: mvd [38,3]/[1,0],
amvd0=0/38, amvd1=0/3, final MV [38,3]/[39,3].

With deblocking disabled, Freh7_B frame 0 first differs at Y(16,1)
(Wedo 232 vs FFmpeg 255); even rows only diverge from y=32, consistent with
bottom-field errors propagating into the paired frame. Since CABAC bins,
contexts, ref counts and MV prediction are identical, the remaining divergence
is in reference sampling, field-view MC or field pairing - outside CABAC.
No patch; FRext stays 44/69 with zero errors.
Evidence: `/tmp/ai-agent/paffmvd2/evidence.md` (ephemeral).

## Appendix Y: pure-PAFF intra8x8 above-left candidate (2026-11-28, no patch)

At `Freh7_B.264` frame 0, the first divergent pixel Y(16,1) (Wedo 232 vs
FFmpeg 255) lies inside bottom-field MB(1,1), which is an intra 8x8DCT MB, not
a motion-compensated block. FFmpeg gdb at `h264_mb.c:651` reported dir=1
(horizontal), topleft_available=0, prediction sum 12896 and first row 242 x8.
Wedeo source-level tracing at the same MB gave prediction sum 12648, i.e. 248
lower = 8 x 31, consistent with `fl[0]` being filtered using a fallback
top_left=128 instead of left[0].

Candidate cause: in `decode_intra8x8`, pure PAFF (`mb_field=true`,
`is_mbaff=false`) still uses frame row `mb_y-1` for above-left/top-right
availability instead of the field row `mb_y-2` that FFmpeg uses. Commit 1f5a1fb
already fixed the analogous MBAFF case. A one-line gate was staged but never
built or verified because the shared build slot was saturated; it was reverted
as instructed. Next round: build and gate this exact change. Evidence:
`/tmp/ai-agent/` PAFF fieldmc logs (ephemeral).
