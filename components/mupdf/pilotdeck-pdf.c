/* Thin native MuPDF 1.27 component. The Rust host owns deadlines, process
 * cancellation, input snapshots and output budgets. No application state or
 * network access belongs here. Distributed under AGPL-3.0-or-later. */
#include <mupdf/fitz.h>
#include <errno.h>
#include <limits.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Private host/component contract; see CONTRACT.md. */
enum { PDF_PAGE_OUT_OF_RANGE = 3 };

int main(int argc, char **argv)
{
    fz_context *ctx;
    fz_document *document = NULL;
    fz_page *page = NULL;
    fz_stext_page *text = NULL;
    fz_pixmap *pixmap = NULL;
    fz_output *output;
    int status = 0;
    long requested = 0;
    int render = argc >= 2 && strcmp(argv[1], "render") == 0;
    if (argc < 3 || (render ? argc != 4 : argc != 3) ||
        (!render && strcmp(argv[1], "count") != 0 && strcmp(argv[1], "text") != 0)) {
        fprintf(stderr, "usage: pilotdeck-pdf count|text FILE | render FILE PAGE\n");
        return 2;
    }
    if (render) {
        char *end;
        errno = 0;
        requested = strtol(argv[3], &end, 10);
        if (errno || *end || requested < 1 || requested > INT_MAX) {
            fprintf(stderr, "Invalid PDF page number.\n");
            return 2;
        }
    }
    ctx = fz_new_context(NULL, NULL, 64 << 20);
    if (!ctx) { fprintf(stderr, "Cannot create MuPDF context.\n"); return 1; }
    fz_var(document);
    fz_var(page);
    fz_var(text);
    fz_var(pixmap);
    fz_var(status);
    fz_try(ctx) {
        fz_register_document_handlers(ctx);
        output = fz_stdout(ctx);
        document = fz_open_document(ctx, argv[2]);
        int pages = fz_count_pages(ctx, document);
        if (strcmp(argv[1], "count") == 0) {
            fz_write_printf(ctx, output, "%d\n", pages);
        } else if (render) {
            if (requested > pages) {
                /* Open/count succeeded. Preserve this exact semantic without
                 * classifying unrelated MuPDF argument/parse/IO errors. */
                fprintf(stderr, "PDF page is out of range.\n");
                status = PDF_PAGE_OUT_OF_RANGE;
            } else {
                page = fz_load_page(ctx, document, (int)requested - 1);
                fz_matrix matrix = fz_scale(100.0f / 72.0f, 100.0f / 72.0f);
                fz_rect box = fz_transform_rect(fz_bound_page(ctx, page), matrix);
                double width = ceil(box.x1) - floor(box.x0);
                double height = ceil(box.y1) - floor(box.y0);
                /* The store limit is a cache limit, not a pixmap allocation limit. */
                if (!isfinite(width) || !isfinite(height) || width < 1 || height < 1 ||
                    width > 32768 || height > 32768 || width * height > 32000000)
                    fz_throw(ctx, FZ_ERROR_ARGUMENT, "PDF page raster exceeds the native component pixel limit.");
                pixmap = fz_new_pixmap_from_page(ctx, page, matrix, fz_device_rgb(ctx), 0);
                fz_write_pixmap_as_jpeg(ctx, output, pixmap, 80, 0);
            }
        } else {
            for (int index = 0; index < pages; ++index) {
                if (index) fz_write_byte(ctx, output, '\n');
                page = fz_load_page(ctx, document, index);
                text = fz_new_stext_page_from_page(ctx, page, NULL);
                fz_print_stext_page_as_text(ctx, output, text);
                fz_drop_stext_page(ctx, text); text = NULL;
                fz_drop_page(ctx, page); page = NULL;
            }
        }
        fz_flush_output(ctx, output);
    }
    fz_always(ctx) {
        fz_drop_pixmap(ctx, pixmap);
        fz_drop_stext_page(ctx, text);
        fz_drop_page(ctx, page);
        fz_drop_document(ctx, document);
    }
    fz_catch(ctx) {
        fprintf(stderr, "%s\n", fz_caught_message(ctx));
        status = 1;
    }
    fz_drop_context(ctx);
    return status;
}
