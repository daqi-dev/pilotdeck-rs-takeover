//! The baseline's image repair path for `read_file`
//! (`sharp(buffer).jpeg({ quality: 90 }).toBuffer()`, readFile.ts:758-768),
//! reproduced byte for byte for PNG input.
//!
//! sharp 0.34.5 hands the decoded image to libvips, which flattens an alpha
//! channel against black and saves through libjpeg-turbo with quality 90,
//! 4:2:0 chroma subsampling (1x1 for grayscale), optimized Huffman tables, no
//! JFIF/EXIF segments and the integer ("islow") DCT. This module is a port of
//! that encoder path: `jccolor` RGB→YCbCr, `jcsample` h2v2 downsampling with
//! edge expansion, `jccoefct` dummy blocks, `jfdctint` + `jcdctmgr`
//! quantization, `jchuff` optimal tables and `jcmarker` segment order. Its
//! output for the bbx fixture is pinned against the baseline's bytes in the
//! tests below.

/// A decoded 8-bit image: 1 (gray) or 3 (RGB) samples per pixel.
pub struct Raster {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub samples: Vec<u8>,
}

/// Decodes a PNG the way libvips' pngload hands it to the JPEG saver:
/// palettes and low bit depths expanded, 16-bit samples reduced to their high
/// byte, and an alpha channel flattened against black (`v * a / 255`).
pub fn decode_png(bytes: &[u8]) -> Result<Raster, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let mut buffer = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    let (width, height) = (info.width as usize, info.height as usize);
    let data = &buffer[..info.buffer_size()];
    let (channels, alpha) = match info.color_type {
        png::ColorType::Grayscale => (1, false),
        png::ColorType::GrayscaleAlpha => (1, true),
        png::ColorType::Rgb => (3, false),
        png::ColorType::Rgba => (3, true),
        png::ColorType::Indexed => return Err("unexpanded palette".into()),
    };
    let stride = channels + usize::from(alpha);
    let mut samples = Vec::with_capacity(width * height * channels);
    for pixel in data.chunks_exact(stride) {
        if alpha {
            let a = u32::from(pixel[channels]);
            for &value in &pixel[..channels] {
                samples.push((u32::from(value) * a / 255) as u8);
            }
        } else {
            samples.extend_from_slice(&pixel[..channels]);
        }
    }
    Ok(Raster {
        width,
        height,
        channels,
        samples,
    })
}

/// Whether a PNG's header (signature and the chunks up to the image data)
/// parses — the cheap check `sharp(buffer).metadata()` performs.
pub fn png_header_ok(bytes: &[u8]) -> bool {
    png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .is_ok()
}

const STD_LUMINANCE: [u32; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const STD_CHROMINANCE: [u32; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// `jpeg_set_quality` → `jpeg_add_quant_table` (force_baseline).
fn quant_table(base: &[u32; 64], quality: u32) -> [u32; 64] {
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    base.map(|value| ((value * scale + 50) / 100).clamp(1, 255))
}

/// `jccolor.c` `rgb_ycc_convert` (SCALEBITS 16).
fn rgb_ycc(r: i64, g: i64, b: i64) -> (i32, i32, i32) {
    const fn fix(x: f64) -> i64 {
        (x * 65536.0 + 0.5) as i64
    }
    const ONE_HALF: i64 = 1 << 15;
    const CBCR_OFFSET: i64 = 128 << 16;
    let y = (fix(0.29900) * r + fix(0.58700) * g + fix(0.11400) * b + ONE_HALF) >> 16;
    let cb =
        (-fix(0.16874) * r - fix(0.33126) * g + fix(0.5) * b + CBCR_OFFSET + ONE_HALF - 1) >> 16;
    let cr =
        (fix(0.5) * r - fix(0.41869) * g - fix(0.08131) * b + CBCR_OFFSET + ONE_HALF - 1) >> 16;
    (y as i32, cb as i32, cr as i32)
}

/// `jfdctint.c` `jpeg_fdct_islow`: output scaled up by 8.
fn fdct_islow(data: &mut [i64; 64]) {
    const CONST_BITS: u32 = 13;
    const PASS1_BITS: u32 = 2;
    const fn f(x: f64) -> i64 {
        (x * 8192.0 + 0.5) as i64
    }
    fn descale(x: i64, n: u32) -> i64 {
        (x + (1 << (n - 1))) >> n
    }
    let pass = |data: &mut [i64; 64], index: &dyn Fn(usize) -> usize, first: bool| {
        let get = |k: usize| data[index(k)];
        let tmp0 = get(0) + get(7);
        let tmp7 = get(0) - get(7);
        let tmp1 = get(1) + get(6);
        let tmp6 = get(1) - get(6);
        let tmp2 = get(2) + get(5);
        let tmp5 = get(2) - get(5);
        let tmp3 = get(3) + get(4);
        let tmp4 = get(3) - get(4);
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        let shift = if first {
            CONST_BITS - PASS1_BITS
        } else {
            CONST_BITS + PASS1_BITS
        };
        let mut out = [0i64; 8];
        if first {
            out[0] = (tmp10 + tmp11) << PASS1_BITS;
            out[4] = (tmp10 - tmp11) << PASS1_BITS;
        } else {
            out[0] = descale(tmp10 + tmp11, PASS1_BITS);
            out[4] = descale(tmp10 - tmp11, PASS1_BITS);
        }
        let z1 = (tmp12 + tmp13) * f(0.541196100);
        out[2] = descale(z1 + tmp13 * f(0.765366865), shift);
        out[6] = descale(z1 + tmp12 * -f(1.847759065), shift);
        let z1 = tmp4 + tmp7;
        let z2 = tmp5 + tmp6;
        let z3 = tmp4 + tmp6;
        let z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * f(1.175875602);
        let tmp4 = tmp4 * f(0.298631336);
        let tmp5 = tmp5 * f(2.053119869);
        let tmp6 = tmp6 * f(3.072711026);
        let tmp7 = tmp7 * f(1.501321110);
        let z1 = z1 * -f(0.899976223);
        let z2 = z2 * -f(2.562915447);
        let z3 = z3 * -f(1.961570560) + z5;
        let z4 = z4 * -f(0.390180644) + z5;
        out[7] = descale(tmp4 + z1 + z3, shift);
        out[5] = descale(tmp5 + z2 + z4, shift);
        out[3] = descale(tmp6 + z2 + z3, shift);
        out[1] = descale(tmp7 + z1 + z4, shift);
        for (k, value) in out.into_iter().enumerate() {
            data[index(k)] = value;
        }
    };
    for row in 0..8 {
        pass(data, &|k| row * 8 + k, true);
    }
    for column in 0..8 {
        pass(data, &|k| column + 8 * k, false);
    }
}

/// `jcdctmgr.c` quantization: round half away from zero of `coef / (8q)`.
fn quantize(coefficients: &[i64; 64], table: &[u32; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for i in 0..64 {
        let divisor = i64::from(table[i]) * 8;
        let value = coefficients[i];
        let magnitude = (value.abs() + (divisor >> 1)) / divisor;
        out[i] = (if value < 0 { -magnitude } else { magnitude }) as i32;
    }
    out
}

fn block_from(plane: &Plane, block_x: usize, block_y: usize, table: &[u32; 64]) -> [i32; 64] {
    let mut data = [0i64; 64];
    for row in 0..8 {
        for column in 0..8 {
            data[row * 8 + column] =
                i64::from(plane.at(block_x * 8 + column, block_y * 8 + row)) - 128;
        }
    }
    fdct_islow(&mut data);
    quantize(&data, table)
}

/// A component plane addressed with libjpeg-turbo's edge expansion (the last
/// column and row repeat).
struct Plane {
    width: usize,
    height: usize,
    values: Vec<i32>,
}
impl Plane {
    fn at(&self, x: usize, y: usize) -> i32 {
        self.values[y.min(self.height - 1) * self.width + x.min(self.width - 1)]
    }
}

struct Component {
    plane: Plane,
    h: usize,
    v: usize,
    table: usize,
    width_in_blocks: usize,
    height_in_blocks: usize,
}

fn nbits(value: i32) -> u32 {
    32 - value.unsigned_abs().leading_zeros()
}

/// Everything `jchuff` encodes: per block the DC difference and the AC
/// run/size symbols, against the component's table index.
#[derive(Clone, Copy)]
enum Symbol {
    Dc {
        table: usize,
        diff: i32,
    },
    Ac {
        table: usize,
        symbol: u8,
        value: Option<i32>,
    },
}

/// `jpeg_gen_optimal_table`.
fn optimal_table(frequencies: &[u64; 256]) -> ([u8; 16], Vec<u8>) {
    let mut freq = [0u64; 257];
    freq[..256].copy_from_slice(frequencies);
    freq[256] = 1;
    let mut codesize = [0usize; 257];
    let mut others = [-1isize; 257];
    loop {
        let mut c1: isize = -1;
        let mut v = u64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 && f <= v {
                v = f;
                c1 = i as isize;
            }
        }
        let mut c2: isize = -1;
        let mut v = u64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 && f <= v && i as isize != c1 {
                v = f;
                c2 = i as isize;
            }
        }
        if c2 < 0 {
            break;
        }
        let (mut c1, mut c2) = (c1 as usize, c2 as usize);
        freq[c1] += freq[c2];
        freq[c2] = 0;
        codesize[c1] += 1;
        while others[c1] >= 0 {
            c1 = others[c1] as usize;
            codesize[c1] += 1;
        }
        others[c1] = c2 as isize;
        codesize[c2] += 1;
        while others[c2] >= 0 {
            c2 = others[c2] as usize;
            codesize[c2] += 1;
        }
    }
    let mut bits = [0i64; 33];
    for &size in &codesize {
        if size > 0 {
            bits[size] += 1;
        }
    }
    for i in (17..=32).rev() {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
    }
    let mut i = 16;
    while bits[i] == 0 {
        i -= 1;
    }
    bits[i] -= 1;
    let mut values = Vec::new();
    for size in 1..=32 {
        for (symbol, &length) in codesize.iter().enumerate().take(256) {
            if length == size {
                values.push(symbol as u8);
            }
        }
    }
    let mut counts = [0u8; 16];
    for (k, count) in counts.iter_mut().enumerate() {
        *count = bits[k + 1] as u8;
    }
    (counts, values)
}

fn codes(counts: &[u8; 16], values: &[u8]) -> [(u32, u32); 256] {
    let mut table = [(0u32, 0u32); 256];
    let mut code = 0u32;
    let mut k = 0;
    for length in 1..=16u32 {
        for _ in 0..counts[length as usize - 1] {
            table[values[k] as usize] = (code, length);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    table
}

struct BitWriter {
    out: Vec<u8>,
    accumulator: u32,
    used: u32,
}
impl BitWriter {
    fn put(&mut self, value: u32, length: u32) {
        for i in (0..length).rev() {
            self.accumulator = (self.accumulator << 1) | ((value >> i) & 1);
            self.used += 1;
            if self.used == 8 {
                let byte = self.accumulator as u8;
                self.out.push(byte);
                if byte == 0xff {
                    self.out.push(0);
                }
                self.accumulator = 0;
                self.used = 0;
            }
        }
    }
    fn finish(mut self) -> Vec<u8> {
        while self.used != 0 {
            self.put(1, 1);
        }
        self.out
    }
}

/// libvips `jpegsave` at quality 90 through libjpeg-turbo, as sharp drives it.
pub fn encode_jpeg_q90(raster: &Raster) -> Vec<u8> {
    let quality = 90;
    let tables = [
        quant_table(&STD_LUMINANCE, quality),
        quant_table(&STD_CHROMINANCE, quality),
    ];
    let (width, height) = (raster.width, raster.height);
    let color = raster.channels == 3;
    let mut components = Vec::new();
    if color {
        let mut y = Vec::with_capacity(width * height);
        let mut cb = Vec::with_capacity(width * height);
        let mut cr = Vec::with_capacity(width * height);
        for pixel in raster.samples.as_chunks::<3>().0 {
            let (l, b, r) = rgb_ycc(pixel[0].into(), pixel[1].into(), pixel[2].into());
            y.push(l);
            cb.push(b);
            cr.push(r);
        }
        components.push(Component {
            plane: Plane {
                width,
                height,
                values: y,
            },
            h: 2,
            v: 2,
            table: 0,
            width_in_blocks: width.div_ceil(8),
            height_in_blocks: height.div_ceil(8),
        });
        // `h2v2_downsample` over the edge-expanded full-resolution plane:
        // `(a + b + c + d + bias) >> 2`, bias alternating 1, 2 per column.
        let chroma_width = width.div_ceil(16) * 8;
        let chroma_rows = height.div_ceil(2);
        for full in [cb, cr] {
            let full = Plane {
                width,
                height,
                values: full,
            };
            let mut values = Vec::with_capacity(chroma_width * chroma_rows);
            for row in 0..chroma_rows {
                let mut bias = 1;
                for column in 0..chroma_width {
                    let sum = full.at(2 * column, 2 * row)
                        + full.at(2 * column + 1, 2 * row)
                        + full.at(2 * column, 2 * row + 1)
                        + full.at(2 * column + 1, 2 * row + 1);
                    values.push((sum + bias) >> 2);
                    bias ^= 3;
                }
            }
            components.push(Component {
                plane: Plane {
                    width: chroma_width,
                    height: chroma_rows,
                    values,
                },
                h: 1,
                v: 1,
                table: 1,
                width_in_blocks: width.div_ceil(16),
                height_in_blocks: height.div_ceil(16),
            });
        }
    } else {
        components.push(Component {
            plane: Plane {
                width,
                height,
                values: raster.samples.iter().map(|&v| i32::from(v)).collect(),
            },
            h: 1,
            v: 1,
            table: 0,
            width_in_blocks: width.div_ceil(8),
            height_in_blocks: height.div_ceil(8),
        });
    }
    let max_h = components.iter().map(|c| c.h).max().unwrap_or(1);
    let max_v = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcus_per_row = width.div_ceil(8 * max_h);
    let mcu_rows = height.div_ceil(8 * max_v);
    // `jccoefct.c compress_data`: real blocks are transformed; blocks past
    // the component's last block column or row are dummies carrying the
    // previous block's DC.
    let mut symbols: Vec<Symbol> = Vec::new();
    let mut last_dc = vec![0i32; components.len()];
    for mcu_y in 0..mcu_rows {
        for mcu_x in 0..mcus_per_row {
            for (index, component) in components.iter().enumerate() {
                let table = &tables[component.table];
                let last_col_width = match component.width_in_blocks % component.h {
                    0 => component.h,
                    n => n,
                };
                let last_row_height = match component.height_in_blocks % component.v {
                    0 => component.v,
                    n => n,
                };
                let mut mcu_blocks: Vec<[i32; 64]> = Vec::new();
                for y_index in 0..component.v {
                    let block_count = if mcu_x + 1 < mcus_per_row {
                        component.h
                    } else {
                        last_col_width
                    };
                    if mcu_y + 1 < mcu_rows || y_index < last_row_height {
                        for x_index in 0..component.h {
                            if x_index < block_count {
                                mcu_blocks.push(block_from(
                                    &component.plane,
                                    mcu_x * component.h + x_index,
                                    mcu_y * component.v + y_index,
                                    table,
                                ));
                            } else {
                                let mut dummy = [0i32; 64];
                                dummy[0] = mcu_blocks.last().map_or(0, |b| b[0]);
                                mcu_blocks.push(dummy);
                            }
                        }
                    } else {
                        let dc = mcu_blocks.last().map_or(0, |b| b[0]);
                        for _ in 0..component.h {
                            let mut dummy = [0i32; 64];
                            dummy[0] = dc;
                            mcu_blocks.push(dummy);
                        }
                    }
                }
                let huffman = usize::from(component.table != 0);
                for block in mcu_blocks {
                    symbols.push(Symbol::Dc {
                        table: huffman,
                        diff: block[0] - last_dc[index],
                    });
                    last_dc[index] = block[0];
                    let mut run = 0u8;
                    for &position in &ZIGZAG[1..] {
                        let value = block[position];
                        if value == 0 {
                            run += 1;
                            continue;
                        }
                        while run > 15 {
                            symbols.push(Symbol::Ac {
                                table: huffman,
                                symbol: 0xf0,
                                value: None,
                            });
                            run -= 16;
                        }
                        symbols.push(Symbol::Ac {
                            table: huffman,
                            symbol: (run << 4) + nbits(value) as u8,
                            value: Some(value),
                        });
                        run = 0;
                    }
                    if run > 0 {
                        symbols.push(Symbol::Ac {
                            table: huffman,
                            symbol: 0,
                            value: None,
                        });
                    }
                }
            }
        }
    }
    let table_count = if color { 2 } else { 1 };
    let mut dc_freq = vec![[0u64; 256]; table_count];
    let mut ac_freq = vec![[0u64; 256]; table_count];
    for symbol in &symbols {
        match *symbol {
            Symbol::Dc { table, diff } => dc_freq[table][nbits(diff) as usize] += 1,
            Symbol::Ac { table, symbol, .. } => ac_freq[table][usize::from(symbol)] += 1,
        }
    }
    let dc_tables: Vec<_> = dc_freq.iter().map(optimal_table).collect();
    let ac_tables: Vec<_> = ac_freq.iter().map(optimal_table).collect();
    let dc_codes: Vec<_> = dc_tables.iter().map(|(c, v)| codes(c, v)).collect();
    let ac_codes: Vec<_> = ac_tables.iter().map(|(c, v)| codes(c, v)).collect();
    let mut writer = BitWriter {
        out: Vec::new(),
        accumulator: 0,
        used: 0,
    };
    let put_value = |writer: &mut BitWriter, value: i32, length: u32| {
        if length > 0 {
            let bits = if value < 0 { value - 1 } else { value };
            writer.put(bits as u32 & ((1u32 << length) - 1), length);
        }
    };
    for symbol in &symbols {
        match *symbol {
            Symbol::Dc { table, diff } => {
                let length = nbits(diff);
                let (code, size) = dc_codes[table][length as usize];
                writer.put(code, size);
                put_value(&mut writer, diff, length);
            }
            Symbol::Ac {
                table,
                symbol,
                value,
            } => {
                let (code, size) = ac_codes[table][usize::from(symbol)];
                writer.put(code, size);
                if let Some(value) = value {
                    put_value(&mut writer, value, u32::from(symbol & 15));
                }
            }
        }
    }
    let scan = writer.finish();

    let mut out = vec![0xff, 0xd8];
    for (id, table) in tables.iter().take(table_count).enumerate() {
        out.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, id as u8]);
        out.extend(ZIGZAG.iter().map(|&k| table[k] as u8));
    }
    let length = 8 + 3 * components.len();
    out.extend_from_slice(&[0xff, 0xc0, (length >> 8) as u8, length as u8, 8]);
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.push(components.len() as u8);
    for (index, component) in components.iter().enumerate() {
        out.extend_from_slice(&[
            index as u8 + 1,
            ((component.h << 4) | component.v) as u8,
            component.table as u8,
        ]);
    }
    for table in 0..table_count {
        for (class, (counts, values)) in [(0u8, &dc_tables[table]), (0x10, &ac_tables[table])] {
            let length = 2 + 1 + 16 + values.len();
            out.extend_from_slice(&[0xff, 0xc4, (length >> 8) as u8, length as u8]);
            out.push(class | table as u8);
            out.extend_from_slice(counts);
            out.extend_from_slice(values);
        }
    }
    let length = 6 + 2 * components.len();
    out.extend_from_slice(&[0xff, 0xda, (length >> 8) as u8, length as u8]);
    out.push(components.len() as u8);
    for (index, component) in components.iter().enumerate() {
        let table = component.table as u8;
        out.extend_from_slice(&[index as u8 + 1, (table << 4) | table]);
    }
    out.extend_from_slice(&[0, 63, 0]);
    out.extend(scan);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    /// The bbx `gap-tool-read-file-vision` fixture: a 1x1 RGBA PNG
    /// (0, 0, 255, 127) and the JPEG the baseline (sharp 0.34.5) sent for it.
    #[test]
    fn one_pixel_png_matches_the_baseline_bytes() {
        let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
            .unwrap();
        let expected = STANDARD
            .decode("/9j/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAABAAEDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAn/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAABQj/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCZ4CqAj//Z")
            .unwrap();
        let raster = decode_png(&png).unwrap();
        assert_eq!(encode_jpeg_q90(&raster), expected);
    }

    /// PNGs encoded by sharp 0.34.5 (`sharp(raw).png()`) and the JPEG
    /// `sharp(png).jpeg({ quality: 90 })` produced for each: RGB, RGBA,
    /// gray and gray+alpha, with partial MCUs on both edges.
    #[test]
    fn sharp_generated_fixtures_match_byte_for_byte() {
        let fixtures =
            pilotdeck_protocol::parse_json(include_str!("image_codec_fixtures.json")).unwrap();
        let pilotdeck_protocol::JsValue::Array(cases) = &fixtures else {
            panic!("fixture array");
        };
        assert_eq!(cases.len(), 6);
        for case in cases {
            let field = |key: &str| {
                String::from_utf16_lossy(case.get(key).unwrap().as_string().unwrap().units())
            };
            let png = STANDARD.decode(field("png")).unwrap();
            let jpeg = STANDARD.decode(field("jpeg")).unwrap();
            let raster = decode_png(&png).unwrap();
            assert_eq!(encode_jpeg_q90(&raster), jpeg, "{}", field("name"));
        }
    }
}
