/* scryjpeg — minimal RGB decode shim over mozjpeg 4.1.5's RAW libjpeg API,
 * configured exactly like libvips jpeg2vips.c (library defaults, scale 1/1,
 * per-row scanlines). scrybox-server dlopens this and gets BIT-IDENTICAL
 * decodes to the container's sharp/libvips/mozjpeg.
 *
 * build:
 *   gcc -O2 -fPIC -shared -I<moz415> scryjpeg.c <moz415>/build/libjpeg.a \
 *       -lm -o libscryjpeg.so
 */
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <setjmp.h>
#include "jpeglib.h"

typedef struct {
    struct jpeg_error_mgr pub;
    jmp_buf jump;
} scry_err;

static void scry_error(j_common_ptr cinfo)
{
    longjmp(((scry_err *) cinfo->err)->jump, 1);
}

/* Decode JPEG -> malloc'd RGB. Returns 0 on success. Caller frees *pixels. */
int scry_jpeg_decode(const unsigned char *data, long len,
                     int *width, int *height, unsigned char **pixels)
{
    struct jpeg_decompress_struct cinfo;
    scry_err jerr;

    cinfo.err = jpeg_std_error(&jerr.pub);
    jerr.pub.error_exit = scry_error;
    if (setjmp(jerr.jump)) {
        jpeg_destroy_decompress(&cinfo);
        return -1;
    }
    jpeg_create_decompress(&cinfo);
    jpeg_mem_src(&cinfo, data, len);
    (void) jpeg_read_header(&cinfo, TRUE);
    /* Pin the IDCT explicitly: mozjpeg's default path enables quantization
     * block-smoothing unless dct_method is set, and the container's
     * libvips-bundled libjpeg decodes WITHOUT it (pixel-verified: setting
     * JDCT_ISLOW makes scan1-photo decode bit-identical, sumabs 0). */
    cinfo.dct_method = JDCT_ISLOW;
    cinfo.scale_num = 1;
    cinfo.scale_denom = 1;
    jpeg_calc_output_dimensions(&cinfo);
    (void) jpeg_start_decompress(&cinfo);

    int w = (int) cinfo.output_width;
    int comps = (int) cinfo.output_components;
    if (comps != 1 && comps != 3) {
        /* CMYK/other: let the caller fall back (matches nothing in the lane). */
        jpeg_destroy_decompress(&cinfo);
        return -2;
    }
    unsigned char *buf = (unsigned char *) malloc((size_t) w * cinfo.output_height * 3);
    if (!buf) {
        jpeg_destroy_decompress(&cinfo);
        return -3;
    }
    JSAMPROW row;
    unsigned char *dst = buf;
    while ((int) cinfo.output_scanline < (int) cinfo.output_height) {
        row = dst;
        (void) jpeg_read_scanlines(&cinfo, &row, 1);
        if (comps == 1) {
            /* grayscale -> replicate to RGB along the row (read in place) */
            unsigned char *base = dst; /* row was written at dst[0..w) */
            for (int x = w - 1; x >= 0; x--) {
                base[x * 3] = base[x * 3 + 1] = base[x * 3 + 2] = base[x];
            }
        }
        dst += w * 3;
    }
    (void) jpeg_finish_decompress(&cinfo);
    jpeg_destroy_decompress(&cinfo);
    *width = w;
    *height = (int) cinfo.image_height;
    *pixels = buf;
    return 0;
}
