//! decoded kitty graphics images + chunked-transmission reassembly. raw RGB
//! (f=24) and RGBA (f=32) only; PNG (f=100) is a deferred fast-follow

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
        let id = if id == 0 {
            self.next_auto = self.next_auto.wrapping_add(1).max(1);
            self.next_auto
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
            return None;
        }
        if more {
            return None;
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
}

/// decode raw RGB/RGBA into RGBA8; None on an unsupported format or short data
fn decode(format: u32, w: u32, h: u32, data: &[u8]) -> Option<Image> {
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
        _ => return None, // PNG (100) deferred
    };
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
}
