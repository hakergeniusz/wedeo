# FFmpeg H.264 PAFF (Picture Adaptive Frame/Field) Implementation Reference

Source: `/var/cache/zstd5/ffmpeg-src-w62/libavcodec/` (FFmpeg 8.0.git, commit `05c674c913`)

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

**Still missing (do not re-attempt partial patch without ground truth):**
- PAFF top/bottom pairing in `process_nal` (bottom field must reuse the top
  field's FDC and DPB slot, with one combined output frame).
- Field MB row addressing in the CABAC/CAVLC loops (`mb_y = 2*field_y + parity`).
- Field-view MC (`pic_as_field`: bottom data offset by one line, doubled stride,
  field-coordinate `ref_y`) in `apply_mc_partition` and `reconstruct_inter_mb_hires`.
- Field-aware deblocking.

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
