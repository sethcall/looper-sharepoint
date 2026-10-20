//! QuickXorHash, the content hash SharePoint Online guarantees on every file.
//!
//! Port of Microsoft's reference algorithm:
//! <https://learn.microsoft.com/en-us/onedrive/developer/code-snippets/quickxorhash>

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const CELLS: usize = WIDTH_IN_BITS.div_ceil(64); // 3

#[derive(Clone, Default)]
pub struct QuickXorHash {
    data: [u64; CELLS],
    shift_so_far: usize,
    length_so_far: u64,
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut vector_array_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        let iterations = bytes.len().min(WIDTH_IN_BITS);

        for i in 0..iterations {
            let is_last_cell = vector_array_index == CELLS - 1;
            let bits_in_cell = if is_last_cell { WIDTH_IN_BITS % 64 } else { 64 };

            if vector_offset <= bits_in_cell - 8 {
                let mut j = i;
                while j < bytes.len() {
                    self.data[vector_array_index] ^= (bytes[j] as u64) << vector_offset;
                    j += WIDTH_IN_BITS;
                }
            } else {
                let index1 = vector_array_index;
                let index2 = if is_last_cell { 0 } else { vector_array_index + 1 };
                let low = bits_in_cell - vector_offset;
                let mut xored: u8 = 0;
                let mut j = i;
                while j < bytes.len() {
                    xored ^= bytes[j];
                    j += WIDTH_IN_BITS;
                }
                self.data[index1] ^= (xored as u64) << vector_offset;
                self.data[index2] ^= (xored as u64) >> low;
            }

            vector_offset += SHIFT;
            while vector_offset >= bits_in_cell {
                vector_array_index = if is_last_cell { 0 } else { vector_array_index + 1 };
                vector_offset -= bits_in_cell;
            }
        }

        self.shift_so_far = (self.shift_so_far + SHIFT * (bytes.len() % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length_so_far += bytes.len() as u64;
    }

    pub fn finalize(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..8].copy_from_slice(&self.data[0].to_le_bytes());
        out[8..16].copy_from_slice(&self.data[1].to_le_bytes());
        out[16..20].copy_from_slice(&self.data[2].to_le_bytes()[0..4]);
        let len = self.length_so_far.to_le_bytes();
        for (i, b) in len.iter().enumerate() {
            out[12 + i] ^= b;
        }
        out
    }

    /// Base64, as Graph reports it in `file.hashes.quickXorHash`.
    pub fn finalize_base64(&self) -> String {
        STANDARD.encode(self.finalize())
    }
}

/// Hash a file on disk.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = QuickXorHash::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize_base64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(bytes: &[u8]) -> String {
        let mut h = QuickXorHash::new();
        h.update(bytes);
        h.finalize_base64()
    }

    // Expected values were computed by SharePoint Online (uploaded to the QA library, 2026-10-11).
    #[test]
    fn server_vectors() {
        assert_eq!(hash(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(hash(b"a"), "YQAAAAAAAAAAAAAAAQAAAAAAAAA=");
        let v2: Vec<u8> = (0..5).flat_map(|_| 0u8..=255).collect();
        assert_eq!(hash(&v2), "AAAAAAAAAAAAAAAAAAUAAAAAAAA=");
        let v3: Vec<u8> = (0..100_003u32).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        assert_eq!(hash(&v3), "jVj4VoTZMRx5gfdODz0gn/kHw6o=");
    }

    #[test]
    fn chunking_does_not_matter() {
        let v3: Vec<u8> = (0..100_003u32).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        for chunk in [1, 7, 159, 160, 161, 4096, 65_537] {
            let mut h = QuickXorHash::new();
            for c in v3.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.finalize_base64(), "jVj4VoTZMRx5gfdODz0gn/kHw6o=", "chunk {chunk}");
        }
    }
}
