# Overlay for the pinned upstream Makefile; source_root remains read-only.
# make -C MUPDF -f Makefile -f /absolute/path/to/this/Makefile \
#   PILOTDECK_PDF_SOURCE=/absolute/path/to/pilotdeck-pdf.c build=release \
#   HAVE_X11=no HAVE_GLUT=no build/release/pilotdeck-pdf
$(OUT)/pilotdeck-pdf: $(PILOTDECK_PDF_SOURCE) $(MUPDF_LIB) $(THIRD_LIB)
	$(LINK_CMD) $(CFLAGS) $(THIRD_LIBS)
