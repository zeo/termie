//! decoded kitty graphics images + chunked-transmission reassembly. raw RGB
//! (f=24), RGBA (f=32), and PNG (f=100); PNG self-describes its size, so the
//! width/height from the kitty command are ignored for it

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// process-global, monotonic key assigned to each decoded image so the GPU atlas
/// can cache packed images without colliding across panes or re-transmissions
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// a hard cap on a single image's transmitted bytes, so a hostile stream can't
/// grow the reassembly buffer without bound
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// keep at most this many decoded images, evicting the oldest
const MAX_IMAGES: usize = 32;

pub struct Image {
    /// global atlas-cache key (unique per decoded image)
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct Pending {
    format: u32,
    width: u32,
    height: u32,
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
    /// the auto-id of an in-flight anonymous (i=0) chunked transfer, so its
    /// later chunks continue the same image instead of minting a fresh id each
    anon: Option<u32>,
}

impl ImageStore {
    /// feed one transmit chunk; returns Some(id) once an image is fully received
    /// and decoded, or None while more chunks (m=1) are still expected
    pub fn transmit(
        &mut self,
        id: u32,
        format: u32,
        width: u32,
        height: u32,
        more: bool,
        chunk: &[u8],
    ) -> Option<u32> {
        let anonymous = id == 0;
        let id = if anonymous {
            match self.anon {
                Some(a) => a,
                None => {
                    self.next_auto = self.next_auto.wrapping_add(1).max(1);
                    self.next_auto
                }
            }
        } else {
            id
        };
        let p = self.pending.entry(id).or_insert(Pending {
            format: 32,
            width: 0,
            height: 0,
            data: Vec::new(),
        });
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
            self.anon = None;
            return None;
        }
        if more {
            // remember an anonymous transfer so its next chunk continues it
            if anonymous {
                self.anon = Some(id);
            }
            return None;
        }
        if anonymous {
            self.anon = None;
        }
        let p = self.pending.remove(&id)?;
        let mut img = decode(p.format, p.width, p.height, &p.data)?;
        img.key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        if self.images.insert(id, img).is_none() {
            self.order.push(id);
        }
        while self.order.len() > MAX_IMAGES {
            let old = self.order.remove(0);
            self.images.remove(&old);
        }
        Some(id)
    }

    pub fn get(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
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
        self.anon = None;
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
fn decode_png(data: &[u8]) -> Option<Image> {
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
        let id = s.transmit(7, 24, 2, 1, false, &data).expect("decoded");
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
        let id = s.transmit(9, 100, 0, 0, false, &bytes).expect("png decoded");
        let img = s.get(id).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn reassembles_chunks_before_decoding() {
        let mut s = ImageStore::default();
        // 1x1 RGBA split across two chunks
        assert!(s.transmit(3, 32, 1, 1, true, &[1, 2]).is_none());
        let id = s.transmit(3, 0, 0, 0, false, &[3, 4]).expect("decoded");
        assert_eq!(id, 3);
        assert_eq!(s.get(3).unwrap().rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn delete_forgets_the_image() {
        let mut s = ImageStore::default();
        s.transmit(5, 32, 1, 1, false, &[9, 9, 9, 9]);
        assert!(s.get(5).is_some());
        s.delete(5);
        assert!(s.get(5).is_none());
    }

    #[test]
    fn clear_forgets_every_image() {
        let mut s = ImageStore::default();
        s.transmit(1, 32, 1, 1, false, &[1, 1, 1, 1]);
        s.transmit(2, 32, 1, 1, false, &[2, 2, 2, 2]);
        assert!(s.get(1).is_some() && s.get(2).is_some());
        s.clear();
        assert!(s.get(1).is_none() && s.get(2).is_none());
    }

    // an anonymous (i=0) chunked transfer continues into ONE image, not a fresh
    // id per chunk
    #[test]
    fn anon_chunked_continuation_uses_one_id() {
        let mut s = ImageStore::default();
        // a 1x2 RGBA image (8 bytes) split across two anonymous chunks
        assert!(s.transmit(0, 32, 1, 2, true, &[1, 2, 3, 4]).is_none()); // more=true
        let id = s.transmit(0, 0, 0, 0, false, &[5, 6, 7, 8]).expect("completes");
        assert_eq!(s.get(id).unwrap().rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    // the store keeps at most MAX_IMAGES, evicting oldest-first by insertion order
    #[test]
    fn lru_evicts_oldest_first() {
        let mut s = ImageStore::default();
        let n = MAX_IMAGES as u32 + 1;
        for i in 1..=n {
            s.transmit(i, 32, 1, 1, false, &[i as u8, 0, 0, 255]);
        }
        assert!(s.get(1).is_none(), "the oldest image is evicted");
        assert!(s.get(n).is_some(), "the newest image is kept");
    }
}
