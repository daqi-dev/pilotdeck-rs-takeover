# Use only the explicitly prepared source-built libvips prefix.
CXX ?= c++
PKG_CONFIG ?= pkg-config
OUT ?= build/pilotdeck-image
CXXFLAGS ?= -O2
RPATH ?= $$ORIGIN/../lib

$(OUT): pilotdeck-image.cpp
	mkdir -p $(dir $(OUT))
	$(CXX) -std=c++17 $(CPPFLAGS) $(CXXFLAGS) -Wall -Wextra -Werror $< -o $@ $(LDFLAGS) -Wl,-rpath,'$(RPATH)' $$($(PKG_CONFIG) --cflags --libs vips-cpp)
