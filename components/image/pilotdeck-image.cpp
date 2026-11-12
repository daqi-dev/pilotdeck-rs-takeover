// Native image validation/repair and the baseline's three compression passes.
// Build against the pinned libvips 8.17.3 codec set recorded in the manifest.
#include <vips/vips8>
#include <algorithm>
#include <cerrno>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <memory>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

using vips::VImage;
using Bytes = std::vector<unsigned char>;
static constexpr size_t INPUT_LIMIT = 128 * 1024 * 1024;
static constexpr size_t OUTPUT_LIMIT = 32 * 1024 * 1024;
static constexpr double PIXEL_LIMIT = 268402689; // Sharp's default 16383².

struct Encoded {
    Bytes bytes;
    std::string mime;
};

static Bytes read_input(const char *path) {
    std::ifstream stream(path, std::ios::binary | std::ios::ate);
    if (!stream) throw std::runtime_error("Image input could not be opened.");
    auto length = stream.tellg();
    if (length < 0 || static_cast<unsigned long long>(length) > INPUT_LIMIT)
        throw std::runtime_error("Image exceeds the native component input size limit.");
    Bytes bytes(static_cast<size_t>(length));
    stream.seekg(0);
    if (!bytes.empty() && !stream.read(reinterpret_cast<char *>(bytes.data()), bytes.size()))
        throw std::runtime_error("Image input could not be read.");
    return bytes;
}

static VImage load(const Bytes &bytes, int shrink = 1, double scale = 1) {
    auto options = VImage::option()->set("access", VIPS_ACCESS_SEQUENTIAL)
        ->set("fail_on", VIPS_FAIL_ON_WARNING);
    if (shrink != 1) options->set("shrink", shrink);
    if (scale != 1) options->set("scale", scale);
    auto image = VImage::new_from_buffer(bytes.data(), bytes.size(), "", options);
    if (image.width() <= 0 || image.height() <= 0 ||
        static_cast<double>(image.width()) * image.height() > PIXEL_LIMIT)
        throw std::runtime_error("Input image exceeds pixel limit");
    return image;
}

static VImage colour(VImage image) {
    // Sharp applies embedded ICC conversion before resize, then produces sRGB.
    if (vips_image_get_typeof(image.get_image(), VIPS_META_ICC_NAME)) {
        try {
            image = image.icc_transform("srgb", VImage::option()
                ->set("embedded", true)->set("depth", 8)
                ->set("intent", VIPS_INTENT_PERCEPTUAL));
        } catch (const vips::VError &) { vips_error_clear(); }
    } else if (image.interpretation() == VIPS_INTERPRETATION_CMYK) {
        image = image.icc_transform("srgb", VImage::option()
            ->set("input_profile", "cmyk")->set("intent", VIPS_INTENT_PERCEPTUAL));
    }
    return image;
}

static Encoded encode(VImage image, const std::string &mime, int quality) {
    image = image.colourspace(VIPS_INTERPRETATION_sRGB);
    void *data = nullptr;
    size_t size = 0;
    int status;
    if (mime == "image/png") {
        status = vips_pngsave_buffer(image.get_image(), &data, &size,
            "keep", VIPS_FOREIGN_KEEP_NONE, "compression", 9,
            "filter", VIPS_FOREIGN_PNG_FILTER_NONE, "interlace", FALSE,
            "palette", FALSE, "bitdepth", 8, NULL);
    } else if (mime == "image/webp") {
        status = vips_webpsave_buffer(image.get_image(), &data, &size,
            "keep", VIPS_FOREIGN_KEEP_NONE, "Q", quality, "lossless", FALSE,
            "near_lossless", FALSE, "smart_subsample", FALSE, "effort", 4,
            "alpha_q", 100, NULL);
    } else {
        status = vips_jpegsave_buffer(image.get_image(), &data, &size,
            "keep", VIPS_FOREIGN_KEEP_NONE, "Q", quality, "interlace", FALSE,
            "subsample_mode", VIPS_FOREIGN_SUBSAMPLE_ON, "trellis_quant", FALSE,
            "quant_table", 0, "overshoot_deringing", FALSE,
            "optimize_scans", FALSE, "optimize_coding", TRUE, NULL);
    }
    std::unique_ptr<void, decltype(&g_free)> output(data, g_free);
    if (status) throw vips::VError();
    if (size > OUTPUT_LIMIT) throw std::runtime_error("Image operation exceeded its output size limit.");
    auto begin = static_cast<unsigned char *>(data);
    return {Bytes(begin, begin + size), mime};
}

static Encoded resize(const Encoded &input, int edge, int quality,
                      const std::string &mime, bool rotate) {
    auto image = load(input.bytes);
    double ratio = std::max(1.0, std::max(image.width(), image.height()) / static_cast<double>(edge));
    // Use the baseline's fast shrink-on-load selection, including its exact
    // integer-ratio correction, instead of thumbnail's different pre-shrink.
    if (input.mime == "image/jpeg") {
        int shrink = ratio >= 8 ? 8 : ratio >= 4 ? 4 : ratio >= 2 ? 2 : 1;
        if (shrink > 1 && static_cast<int>(ratio) == shrink) shrink /= 2;
        if (shrink > 1) image = load(input.bytes, shrink);
    } else if (input.mime == "image/webp" && ratio > 1) {
        image = load(input.bytes, 1, 1.0 / ratio);
    }
    image = colour(image);
    ratio = std::max(1.0, std::max(image.width(), image.height()) / static_cast<double>(edge));
    // Extreme aspect ratios must retain at least one pixel on each axis.
    double horizontal = 1.0 / std::min(ratio, static_cast<double>(image.width()));
    double vertical = 1.0 / std::min(ratio, static_cast<double>(image.height()));
    bool alpha = image.has_alpha() && (horizontal != 1 || vertical != 1);
    auto format = image.format();
    if (alpha) image = image.premultiply().cast(format);
    if (horizontal != 1 || vertical != 1)
        image = image.resize(horizontal, VImage::option()->set("vscale", vertical)->set("kernel", VIPS_KERNEL_LANCZOS3));
    if (rotate) image = image.autorot();
    if (alpha) image = image.unpremultiply().cast(format);
    return encode(image, mime, quality);
}

static Encoded validate(Encoded input) {
    const bool jpeg = input.mime == "image/jpeg";
    const size_t size = input.bytes.size();
    if (jpeg && size > 1000 && input.bytes[size - 2] == 0xff && input.bytes[size - 1] == 0xd9)
        return input;
    if (!jpeg && size > 1000) {
        try { load(input.bytes); return input; }
        catch (const std::exception &) { vips_error_clear(); }
    }
    try { return encode(colour(load(input.bytes)), "image/jpeg", 90); }
    catch (const std::exception &) {
        throw std::runtime_error("Image file appears truncated or corrupted (" +
            std::to_string(size) + " bytes). Cannot decode.");
    }
}

int main(int argc, char **argv) {
    if (argc != 5 || (std::string(argv[1]) != "prepare" && std::string(argv[1]) != "compress")) {
        std::fprintf(stderr, "usage: pilotdeck-image prepare|compress INPUT MIME MAX_BYTES\n");
        return 2;
    }
    if (VIPS_INIT(argv[0])) { std::fprintf(stderr, "libvips initialization failed\n"); return 1; }
    vips_concurrency_set(1);
    vips_cache_set_max(0);
    vips_cache_set_max_mem(64 * 1024 * 1024);
    int status = 0;
    try {
        if (std::string(vips_version_string()) != "8.17.3")
            throw std::runtime_error("Image component requires pinned libvips 8.17.3.");
        errno = 0;
        char *end = nullptr;
        auto budget = std::strtoull(argv[4], &end, 10);
        if (errno || end == argv[4] || *end || argv[4][0] == '-')
            throw std::runtime_error("Invalid image byte budget.");
        Encoded output{read_input(argv[2]), argv[3]};
        if (std::string(argv[1]) == "prepare") output = validate(std::move(output));
        const std::string original_mime = output.mime;
        if (output.bytes.size() > budget) {
            try {
                const bool gif = output.mime == "image/gif";
                const std::string mime = output.mime == "image/png" || gif ? "image/png" :
                    output.mime == "image/webp" ? "image/webp" : "image/jpeg";
                output = resize(output, gif ? 1200 : 1600, 80, mime, true);
                if (output.bytes.size() > budget) output = resize(output, 1200, 55, "image/jpeg", false);
                if (output.bytes.size() > budget) output = resize(output, 800, 40, "image/jpeg", false);
            } catch (const std::exception &) { vips_error_clear(); }
        }
        if (output.bytes.size() > budget)
            throw std::runtime_error("Image content exceeds the read_file byte budget after compression attempts (" + original_mime + ").");
        if (output.bytes.size() > OUTPUT_LIMIT)
            throw std::runtime_error("Image operation exceeded its output size limit.");
        std::printf("%s\n", output.mime.c_str());
        if (std::fwrite(output.bytes.data(), 1, output.bytes.size(), stdout) != output.bytes.size() || std::fflush(stdout))
            throw std::runtime_error("Image output write failed.");
    } catch (const std::exception &error) {
        std::fprintf(stderr, "%s\n", error.what());
        status = 1;
    }
    vips_shutdown();
    return status;
}
