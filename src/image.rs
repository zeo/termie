//! decoded kitty graphics images + chunked-transmission reassembly. raw RGB
//! (f=24), RGBA (f=32), and PNG (f=100); PNG self-describes its size, so the
//! width/height from the kitty command are ignored for it

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// process-global, monotonic key assigned to each decoded image so the GPU atlas
/// can cache packed images without colliding across panes or re-transmissions
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// reserve a fresh atlas-cache key outside the kitty decode path (the window
/// background image shares the same color-atlas cache as kitty placements)
pub fn alloc_key() -> u64 {
    NEXT_KEY.fetch_add(1, Ordering::Relaxed)
}

/// box-downscale an RGBA image so its long side is at most `max_side`,
/// preserving aspect; the input comes back untouched when it already fits
pub fn downscale_rgba(rgba: &[u8], w: u32, h: u32, max_side: u32) -> (Vec<u8>, u32, u32) {
    if w <= max_side && h <= max_side {
        return (rgba.to_vec(), w, h);
    }
    let scale = max_side as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        let sy0 = (y as u64 * h as u64 / nh as u64) as u32;
        let sy1 = (((y as u64 + 1) * h as u64).div_ceil(nh as u64) as u32).clamp(sy0 + 1, h);
        for x in 0..nw {
            let sx0 = (x as u64 * w as u64 / nw as u64) as u32;
            let sx1 = (((x as u64 + 1) * w as u64).div_ceil(nw as u64) as u32).clamp(sx0 + 1, w);
            let mut acc = [0u64; 4];
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = ((sy * w + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += rgba[i + c] as u64;
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u64;
            out.extend(acc.iter().map(|&a| (a / n) as u8));
        }
    }
    (out, nw, nh)
}

/// a hard cap on a single image's transmitted bytes, so a hostile stream can't
/// grow the reassembly buffer without bound
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// keep at most this many decoded images, evicting the oldest
const MAX_IMAGES: usize = 32;
/// cap on the summed decoded bytes across the store: the per-image and count
/// caps alone still allowed 32 × 64 MB to be pinned by one hostile stream
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// cap on concurrent chunked (m=1) reassembly buffers; each may grow to
/// MAX_IMAGE_BYTES, so unbounded ids would be an allocation amplifier
const MAX_PENDING: usize = 4;

pub struct Image {
    /// global atlas-cache key (unique per decoded image)
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// a display request from an a=T chunk: the c=/r= cell box, whether the
/// cursor steps past the placement (the kitty C= movement policy), and the
/// z= stacking order. `virt` marks a U=1 virtual placement: nothing paints
/// and the cursor holds; placeholder cells reference the box later
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DisplayReq {
    pub cols: u16,
    pub rows: u16,
    pub step: bool,
    pub z: i32,
    pub virt: bool,
}

/// kitty unicode-placeholder base char: a cell of U+10EEEE plus row/column
/// diacritics shows one tile of an image's virtual (U=1) placement
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// kitty's rowcolumn-diacritics.txt: the combining mark at index i encodes
/// row or column number i in a placeholder cell. ascending, for binary search
#[rustfmt::skip]
const ROWCOL_DIACRITICS: [u32; 297] = [
    0x305, 0x30D, 0x30E, 0x310, 0x312, 0x33D, 0x33E, 0x33F, 0x346, 0x34A,
    0x34B, 0x34C, 0x350, 0x351, 0x352, 0x357, 0x35B, 0x363, 0x364, 0x365,
    0x366, 0x367, 0x368, 0x369, 0x36A, 0x36B, 0x36C, 0x36D, 0x36E, 0x36F,
    0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595, 0x597,
    0x598, 0x599, 0x59C, 0x59D, 0x59E, 0x59F, 0x5A0, 0x5A1, 0x5A8, 0x5A9,
    0x5AB, 0x5AC, 0x5AF, 0x5C4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615,
    0x616, 0x617, 0x657, 0x658, 0x659, 0x65A, 0x65B, 0x65D, 0x65E, 0x6D6,
    0x6D7, 0x6D8, 0x6D9, 0x6DA, 0x6DB, 0x6DC, 0x6DF, 0x6E0, 0x6E1, 0x6E2,
    0x6E4, 0x6E7, 0x6E8, 0x6EB, 0x6EC, 0x730, 0x732, 0x733, 0x735, 0x736,
    0x73A, 0x73D, 0x73F, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74A,
    0x7EB, 0x7EC, 0x7ED, 0x7EE, 0x7EF, 0x7F0, 0x7F1, 0x7F3, 0x816, 0x817,
    0x818, 0x819, 0x81B, 0x81C, 0x81D, 0x81E, 0x81F, 0x820, 0x821, 0x822,
    0x823, 0x825, 0x826, 0x827, 0x829, 0x82A, 0x82B, 0x82C, 0x82D, 0x951,
    0x953, 0x954, 0xF82, 0xF83, 0xF86, 0xF87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C,
    0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1,
    0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6,
    0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF,
    0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6,
    0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0,
    0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1,
    0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9,
    0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2,
    0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189,
    0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// the row/column number a placeholder diacritic encodes; None for any other char
pub fn rowcol_index(c: char) -> Option<u16> {
    let cp = c as u32;
    if !(0x305..=0x1D244).contains(&cp) {
        return None;
    }
    ROWCOL_DIACRITICS.binary_search(&cp).ok().map(|i| i as u16)
}

/// what one placeholder cell says about itself: the image id's low bits from
/// the foreground color, and row / column / id-most-significant-byte from the
/// cell's combining diacritics (each may be omitted and inherited from the
/// cell to the left, per the protocol)
pub struct PlaceholderCell {
    pub id_low: u32,
    pub row: Option<u16>,
    pub col: Option<u16>,
    pub msb: Option<u16>,
}

/// decode a placeholder cell from its foreground color and grapheme cluster
/// (empty when the cell carries no diacritics). a default-colored cell names
/// no image and decodes to None
pub fn decode_placeholder(fg: crate::color::Color, cluster: &str) -> Option<PlaceholderCell> {
    let id_low = match fg {
        crate::color::Color::Indexed(n) => n as u32,
        crate::color::Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        _ => return None,
    };
    let mut marks = cluster.chars().skip(1).filter_map(rowcol_index);
    // the third mark names the id's most significant BYTE; table indices past
    // 255 exist for row/column use only and would shift into a bogus id
    Some(PlaceholderCell {
        id_low,
        row: marks.next(),
        col: marks.next(),
        msb: marks.next().filter(|&m| m <= 255),
    })
}

struct Pending {
    format: u32,
    width: u32,
    height: u32,
    /// display request captured from the chunk that carried a=T — continuation
    /// chunks parse with the default action, so the intent must survive until
    /// the transfer completes
    display: Option<DisplayReq>,
    data: Vec<u8>,
}

#[derive(Default)]
pub struct ImageStore {
    images: HashMap<u32, Image>,
    /// insertion order of `images` ids, for simple oldest-first eviction
    order: Vec<u32>,
    /// in-progress chunked (m=1) transmissions keyed by image id
    pending: HashMap<u32, Pending>,
    next_auto: u32,
    /// the id of the in-flight chunked transfer, explicit or auto: kitty
    /// clients name the id only in the first chunk, so an id-less (i=0)
    /// continuation attaches here instead of minting a fresh image
    current: Option<u32>,
}

impl ImageStore {
    /// feed one transmit chunk; returns (id, display request) once an image is
    /// fully received and decoded, or None while more chunks (m=1) are still
    /// expected. `display` is Some on a chunk that asked to show the image
    /// (a=T) and is remembered across the whole chunked transfer
    // the parameters mirror the kitty control keys one-to-one; a struct would
    // only relocate them
    #[allow(clippy::too_many_arguments)]
    pub fn transmit(
        &mut self,
        id: u32,
        format: u32,
        width: u32,
        height: u32,
        more: bool,
        display: Option<DisplayReq>,
        chunk: &[u8],
    ) -> Option<(u32, Option<DisplayReq>)> {
        let anonymous = id == 0;
        let id = if anonymous {
            match self.current {
                Some(a) => a,
                None => {
                    self.next_auto = self.next_auto.wrapping_add(1).max(1);
                    self.next_auto
                }
            }
        } else {
            id
        };
        // refuse to open more reassembly buffers than the cap; continuing an
        // existing transfer is always allowed
        if !self.pending.contains_key(&id) && self.pending.len() >= MAX_PENDING {
            if anonymous {
                self.current = None;
            }
            return None;
        }
        let p = self.pending.entry(id).or_insert(Pending {
            format: 32,
            width: 0,
            height: 0,
            display: None,
            data: Vec::new(),
        });
        if display.is_some() {
            p.display = display;
        }
        if format != 0 {
            p.format = format;
        }
        if width != 0 {
            p.width = width;
        }
        if height != 0 {
            p.height = height;
        }
        p.data.extend_from_slice(chunk);
        if p.data.len() > MAX_IMAGE_BYTES {
            self.pending.remove(&id);
            self.current = None;
            return None;
        }
        if more {
            // remember the transfer so its id-less next chunk continues it
            self.current = Some(id);
            return None;
        }
        if self.current == Some(id) {
            self.current = None;
        }
        let p = self.pending.remove(&id)?;
        let img = decode(p.format, p.width, p.height, &p.data)?;
        let disp = p.display;
        self.store(id, img);
        Some((id, disp))
    }

    /// store an already-decoded RGBA image (the sixel path), minting a fresh id
    /// that never lands on a live entry so it can't evict a kitty image
    pub fn insert(&mut self, width: u32, height: u32, rgba: Vec<u8>) -> u32 {
        loop {
            self.next_auto = self.next_auto.wrapping_add(1).max(1);
            if !self.images.contains_key(&self.next_auto) {
                break;
            }
        }
        let id = self.next_auto;
        self.store(id, Image { key: 0, width, height, rgba });
        id
    }

    fn store(&mut self, id: u32, mut img: Image) {
        img.key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        if self.images.insert(id, img).is_none() {
            self.order.push(id);
        }
        while self.order.len() > MAX_IMAGES
            || (self.order.len() > 1 && self.total_bytes() > MAX_TOTAL_BYTES)
        {
            let old = self.order.remove(0);
            self.images.remove(&old);
        }
    }

    fn total_bytes(&self) -> usize {
        self.images.values().map(|i| i.rgba.len()).sum()
    }

    pub fn get(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    /// the stored image ids inside [lo, hi], for a ranged delete's free pass
    pub fn ids_in(&self, lo: u32, hi: u32) -> Vec<u32> {
        self.images.keys().copied().filter(|id| (lo..=hi).contains(id)).collect()
    }

    /// drop an image (kitty a=d delete), forgetting any decoded pixels
    pub fn delete(&mut self, id: u32) {
        self.images.remove(&id);
        self.order.retain(|&i| i != id);
        self.pending.remove(&id);
    }

    /// drop every decoded image + any in-flight chunked transfer (kitty bare a=d
    /// delete-all), reclaiming the decoded pixel memory
    pub fn clear(&mut self) {
        self.images.clear();
        self.order.clear();
        self.pending.clear();
        self.current = None;
    }
}

/// decode a transmitted image into RGBA8; None on an unsupported format or short
/// data. PNG (f=100) self-describes its size, so it's handled before the w/h
/// guard the raw formats need
fn decode(format: u32, w: u32, h: u32, data: &[u8]) -> Option<Image> {
    if format == 100 {
        return decode_png(data);
    }
    if w == 0 || h == 0 {
        return None;
    }
    let px = (w as usize).checked_mul(h as usize)?;
    let rgba = match format {
        32 => {
            let n = px.checked_mul(4)?;
            if data.len() < n {
                return None;
            }
            data[..n].to_vec()
        }
        24 => {
            let n = px.checked_mul(3)?;
            if data.len() < n {
                return None;
            }
            let mut v = Vec::with_capacity(px * 4);
            for c in data[..n].chunks_exact(3) {
                v.extend_from_slice(c);
                v.push(255);
            }
            v
        }
        _ => return None,
    };
    Some(Image { key: 0, width: w, height: h, rgba })
}

/// decode a PNG into RGBA8 using the `png` crate; width/height come from the PNG
/// header. EXPAND|STRIP_16 normalizes paletted / low-bit / 16-bit down to 8-bit,
/// leaving grayscale / grayscale-alpha / rgb / rgba, which we widen to RGBA8. the
/// decoded-size guard rejects a decompression bomb before allocating
pub(crate) fn decode_png(data: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let sz = reader.output_buffer_size()?;
    if sz == 0 || sz > MAX_IMAGE_BYTES {
        return None;
    }
    let mut buf = vec![0u8; sz];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = (w as usize).checked_mul(h as usize)?;
    let n4 = px.checked_mul(4)?;
    let src = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => {
            if src.len() < n4 {
                return None;
            }
            src[..n4].to_vec()
        }
        png::ColorType::Rgb => {
            let mut v = Vec::with_capacity(n4);
            for c in src.chunks_exact(3) {
                v.extend_from_slice(c);
                v.push(255);
            }
            v
        }
        png::ColorType::GrayscaleAlpha => {
            let mut v = Vec::with_capacity(n4);
            for c in src.chunks_exact(2) {
                v.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            v
        }
        png::ColorType::Grayscale => {
            let mut v = Vec::with_capacity(n4);
            for &g in src {
                v.extend_from_slice(&[g, g, g, 255]);
            }
            v
        }
        _ => return None,
    };
    if rgba.len() != n4 {
        return None;
    }
    Some(Image { key: 0, width: w, height: h, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgb_to_rgba_in_one_shot() {
        let mut s = ImageStore::default();
        // a 2x1 RGB image: red, green
        let data = [255, 0, 0, 0, 255, 0];
        let (id, disp) = s.transmit(7, 24, 2, 1, false, None, &data).expect("decoded");
        assert_eq!(disp, None);
        assert_eq!(id, 7);
        let img = s.get(7).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn decodes_png_via_format_100() {
        // encode a 2x1 RGBA image (red, green) to PNG, then decode it back through
        // transmit(f=100) — kitty sends no width/height for PNG; the header carries it
        let mut bytes: Vec<u8> = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        }
        let mut s = ImageStore::default();
        let (id, _) = s.transmit(9, 100, 0, 0, false, None, &bytes).expect("png decoded");
        let img = s.get(id).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn reassembles_chunks_before_decoding() {
        let mut s = ImageStore::default();
        // 1x1 RGBA split across two chunks
        assert!(s.transmit(3, 32, 1, 1, true, None, &[1, 2]).is_none());
        let (id, _) = s.transmit(3, 0, 0, 0, false, None, &[3, 4]).expect("decoded");
        assert_eq!(id, 3);
        assert_eq!(s.get(3).unwrap().rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn delete_forgets_the_image() {
        let mut s = ImageStore::default();
        s.transmit(5, 32, 1, 1, false, None, &[9, 9, 9, 9]);
        assert!(s.get(5).is_some());
        s.delete(5);
        assert!(s.get(5).is_none());
    }

    #[test]
    fn clear_forgets_every_image() {
        let mut s = ImageStore::default();
        s.transmit(1, 32, 1, 1, false, None, &[1, 1, 1, 1]);
        s.transmit(2, 32, 1, 1, false, None, &[2, 2, 2, 2]);
        assert!(s.get(1).is_some() && s.get(2).is_some());
        s.clear();
        assert!(s.get(1).is_none() && s.get(2).is_none());
    }

    // box-downscale halves a checkerboard into averaged pixels and leaves an
    // already-small image untouched
    #[test]
    fn downscale_averages_and_preserves_small_images() {
        // 2x2: white, black / black, white -> one mid-gray pixel
        let src = [255u8, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255];
        let (out, w, h) = downscale_rgba(&src, 2, 2, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[..3], &[127, 127, 127]);
        assert_eq!(out[3], 255);
        // fits already: bytes come back unchanged
        let (same, w, h) = downscale_rgba(&src, 2, 2, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(same, src);
        // aspect is preserved on the long side
        let wide = vec![9u8; 8 * 2 * 4];
        let (_, w, h) = downscale_rgba(&wide, 8, 2, 4);
        assert_eq!((w, h), (4, 1));
    }

    // continuation chunks carry no i= key (kitty clients name the id only in
    // the first chunk); they must continue an explicit-id transfer, not mint
    // a fresh anonymous image that leaves the named one pending forever
    #[test]
    fn explicit_id_chunks_continue_without_repeating_the_id() {
        let mut s = ImageStore::default();
        assert!(s.transmit(42, 32, 1, 2, true, None, &[1, 2, 3, 4]).is_none());
        let (id, _) = s.transmit(0, 0, 0, 0, false, None, &[5, 6, 7, 8]).expect("completes");
        assert_eq!(id, 42);
        assert_eq!(s.get(42).unwrap().rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    // an anonymous (i=0) chunked transfer continues into ONE image, not a fresh
    // id per chunk
    #[test]
    fn anon_chunked_continuation_uses_one_id() {
        let mut s = ImageStore::default();
        // a 1x2 RGBA image (8 bytes) split across two anonymous chunks
        assert!(s.transmit(0, 32, 1, 2, true, None, &[1, 2, 3, 4]).is_none()); // more=true
        let (id, _) = s.transmit(0, 0, 0, 0, false, None, &[5, 6, 7, 8]).expect("completes");
        assert_eq!(s.get(id).unwrap().rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    // the store keeps at most MAX_IMAGES, evicting oldest-first by insertion order
    #[test]
    fn lru_evicts_oldest_first() {
        let mut s = ImageStore::default();
        let n = MAX_IMAGES as u32 + 1;
        for i in 1..=n {
            s.transmit(i, 32, 1, 1, false, None, &[i as u8, 0, 0, 255]);
        }
        assert!(s.get(1).is_none(), "the oldest image is evicted");
        assert!(s.get(n).is_some(), "the newest image is kept");
    }

    // summed decoded bytes stay under the total budget even when every image
    // is individually legal — the count cap alone allowed 32 × 64 MB
    #[test]
    fn total_byte_budget_evicts_before_count_cap() {
        let mut s = ImageStore::default();
        // 5 images × 64 MB would be 320 MB; the 256 MB budget holds 4
        let px = 4096u32; // 4096×4096 RGBA = 64 MB
        let big = vec![0u8; (px * px * 4) as usize];
        for i in 1..=5u32 {
            s.transmit(i, 32, px, px, false, None, &big);
        }
        assert!(s.get(1).is_none(), "oldest evicted to stay inside the byte budget");
        assert!(s.get(5).is_some());
        assert!(s.total_bytes() <= MAX_TOTAL_BYTES);
    }

    // the rowcolumn table maps kitty placeholder diacritics to their index
    #[test]
    fn rowcol_diacritics_decode_to_indices() {
        assert_eq!(rowcol_index('\u{305}'), Some(0));
        assert_eq!(rowcol_index('\u{30D}'), Some(1));
        assert_eq!(rowcol_index('\u{30E}'), Some(2));
        assert_eq!(rowcol_index('\u{1D244}'), Some(296));
        assert_eq!(rowcol_index('a'), None);
        assert_eq!(rowcol_index('\u{306}'), None); // combining, but not in the table
    }

    #[test]
    fn placeholder_cell_decodes_colors_and_marks() {
        use crate::color::Color;
        // rgb fg carries a 24-bit id; marks give row 1, col 2, id msb 3
        let s = format!("{PLACEHOLDER}\u{30D}\u{30E}\u{310}");
        let p = decode_placeholder(Color::Rgb(0, 0, 42), &s).expect("decodes");
        assert_eq!((p.id_low, p.row, p.col, p.msb), (42, Some(1), Some(2), Some(3)));
        // indexed fg is an 8-bit id; a bare cell leaves everything to inherit
        let p = decode_placeholder(Color::Indexed(7), "").expect("decodes");
        assert_eq!((p.id_low, p.row, p.col, p.msb), (7, None, None, None));
        // a default-colored placeholder names no image
        assert!(decode_placeholder(Color::Default, "").is_none());
        // a third mark past index 255 is row/column vocabulary, not an msb byte
        let s = format!("{PLACEHOLDER}\u{30D}\u{30E}\u{1D244}");
        let p = decode_placeholder(Color::Indexed(7), &s).expect("decodes");
        assert_eq!(p.msb, None);
    }

    // opening reassembly buffers beyond the cap is refused; finishing or
    // continuing existing transfers still works
    #[test]
    fn pending_reassembly_buffers_are_capped() {
        let mut s = ImageStore::default();
        for i in 1..=MAX_PENDING as u32 {
            assert!(s.transmit(i, 32, 1, 1, true, None, &[1, 2]).is_none());
        }
        // a fifth concurrent transfer is dropped outright
        let over = MAX_PENDING as u32 + 1;
        assert!(s.transmit(over, 32, 1, 1, true, None, &[1, 2]).is_none());
        assert!(
            s.transmit(over, 0, 0, 0, false, None, &[3, 4]).is_none(),
            "rejected id never completes"
        );
        // the in-cap transfers all complete
        for i in 1..=MAX_PENDING as u32 {
            assert_eq!(s.transmit(i, 0, 0, 0, false, None, &[3, 4]), Some((i, None)));
        }
    }
}
