//! Image type sniffing (port of pi's `utils/mime.ts detectSupportedImageMimeType`).
//!
//! Identifies supported inline image types (jpeg, png, gif, webp, bmp) from
//! magic bytes. Returns `None` for CMYK JPEG (`ffd8fff7`), animated PNG (acTL),
//! and non-IHDR PNG.

use std::path::Path;

const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;
const PNG_SIGNATURE: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

fn bytes_start_with(buf: &[u8], prefix: &[u8]) -> bool {
    if buf.len() < prefix.len() {
        return false;
    }
    buf.starts_with(prefix)
}

fn starts_with_ascii(buf: &[u8], offset: usize, text: &str) -> bool {
    buf.get(offset..).is_some_and(|s| s.starts_with(text.as_bytes()))
}

fn read_uint16_le(buf: &[u8], offset: usize) -> usize {
    let b = |i: usize| -> usize { buf.get(i).copied().map(|x| x as usize).unwrap_or(0) };
    b(offset) + (b(offset + 1) << 8)
}

fn read_uint32_le(buf: &[u8], offset: usize) -> usize {
    let b = |i: usize| -> usize { buf.get(i).copied().map(|x| x as usize).unwrap_or(0) };
    b(offset) + (b(offset + 1) << 8) + (b(offset + 2) << 16) + b(offset + 3) * 0x1000000
}

fn read_uint32_be(buf: &[u8], offset: usize) -> usize {
    let b = |i: usize| -> usize { buf.get(i).copied().map(|x| x as usize).unwrap_or(0) };
    b(offset) * 0x1000000 + (b(offset + 1) << 16) + (b(offset + 2) << 8) + b(offset + 3)
}

fn is_png(buf: &[u8]) -> bool {
    buf.len() >= 16 && read_uint32_be(buf, PNG_SIGNATURE.len()) == 13 && starts_with_ascii(buf, 12, "IHDR")
}

fn is_animated_png(buf: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buf.len() {
        let chunk_length = read_uint32_be(buf, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buf, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buf, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length + 4;
        if next_offset <= offset || next_offset > buf.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

/// Validates the BMP magic + DIB header (port of `utils/mime.ts isBmp`). Requires
/// `colorPlanes==1` and `bitsPerPixel` in `{1,4,8,16,24,32}`, and applies the
/// declared-file-size / pixel-data-offset sanity checks.
fn is_bmp(buf: &[u8]) -> bool {
    if buf.len() < 26 {
        return false;
    }
    let declared_file_size = read_uint32_le(buf, 2);
    let pixel_data_offset = read_uint32_le(buf, 10);
    let dib_header_size = read_uint32_le(buf, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_uint16_le(buf, 22), read_uint16_le(buf, 24))
    } else if dib_header_size >= 40 && dib_header_size <= 124 {
        if buf.len() < 30 {
            return false;
        }
        (read_uint16_le(buf, 26), read_uint16_le(buf, 28))
    } else {
        return false;
    };
    if color_planes != 1 {
        return false;
    }
    matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

/// Sniffs magic bytes to identify a supported image type. Returns `None` for
/// CMYK JPEG, animated PNG, and non-IHDR PNG.
pub(crate) fn detect_supported_image_mime_type(buf: &[u8]) -> Option<String> {
    if bytes_start_with(buf, &[0xff, 0xd8, 0xff]) {
        if buf.len() > 3 && buf[3] == 0xf7 {
            return None;
        }
        return Some("image/jpeg".to_string());
    }
    if bytes_start_with(buf, PNG_SIGNATURE) {
        if is_png(buf) && !is_animated_png(buf) {
            return Some("image/png".to_string());
        }
        return None;
    }
    if starts_with_ascii(buf, 0, "GIF") {
        return Some("image/gif".to_string());
    }
    if starts_with_ascii(buf, 0, "RIFF") && starts_with_ascii(buf, 8, "WEBP") {
        return Some("image/webp".to_string());
    }
    if starts_with_ascii(buf, 0, "BM") && is_bmp(buf) {
        return Some("image/bmp".to_string());
    }
    None
}

/// Reads up to the sniff window from a file and identifies a supported image
/// type (`detectSupportedImageMimeTypeFromFile`).
pub(crate) fn detect_supported_image_mime_type_from_file(path: &Path) -> Option<String> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return None,
    };
    let mut buf = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
    // Fill the sniff window across short reads (pipes, network FS) — same contract
    // as Go's `io.ReadFull`. EOF/short-at-EOF just means the file is smaller than
    // the window; other errors fail the sniff.
    let n = match read_to_cap(&mut f, &mut buf) {
        Ok(n) => n,
        Err(_) => return None,
    };
    buf.truncate(n);
    detect_supported_image_mime_type(&buf)
}

/// Reads into `buf` until it is full or EOF, retrying short reads. Returns the
/// number of bytes placed in `buf` (0..=buf.len()). Mirrors `io.ReadFull` with
/// EOF/ErrUnexpectedEOF treated as a successful short fill.
fn read_to_cap(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};

    /// Reader that returns at most `chunk` bytes per `read`, exercising short
    /// reads the way a pipe or network FS would.
    struct Chunked<'a> {
        data: &'a [u8],
        pos: usize,
        chunk: usize,
    }

    impl Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn minimal_png() -> Vec<u8> {
        // Signature + IHDR length (13) + "IHDR" + minimal padding so offset-12
        // checks are meaningful under short reads.
        let mut buf = PNG_SIGNATURE.to_vec();
        buf.extend_from_slice(&[0, 0, 0, 13]);
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&[0; 4]);
        buf
    }

    #[test]
    fn detect_png() {
        assert_eq!(detect_supported_image_mime_type(&minimal_png()).as_deref(), Some("image/png"));
    }

    #[test]
    fn detect_jpeg() {
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]).as_deref(),
            Some("image/jpeg")
        );
        // CMYK JPEG (ffd8fff7) is unsupported.
        assert_eq!(detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]), None);
    }

    #[test]
    fn detect_gif_webp() {
        assert_eq!(detect_supported_image_mime_type(b"GIF89a").as_deref(), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(detect_supported_image_mime_type(&webp).as_deref(), Some("image/webp"));
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_supported_image_mime_type(b"plain text"), None);
    }

    #[test]
    fn read_to_cap_fills_across_short_reads() {
        let data = minimal_png();
        let mut r = Chunked { data: &data, pos: 0, chunk: 1 };
        let mut buf = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
        let n = read_to_cap(&mut r, &mut buf).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(&buf[..n], data.as_slice());
        assert_eq!(detect_supported_image_mime_type(&buf[..n]).as_deref(), Some("image/png"));
    }

    #[test]
    fn short_reads_still_identify_png_from_file_path() {
        // End-to-end: write a real PNG header file and sniff via the public
        // from-file path (normal FS read is usually one-shot; the fill loop is
        // still what the path runs).
        let dir = std::env::temp_dir().join(format!("pi-mime-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.png");
        std::fs::write(&path, minimal_png()).unwrap();
        assert_eq!(
            detect_supported_image_mime_type_from_file(&path).as_deref(),
            Some("image/png")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_reads_fill_full_sniff_window_when_larger() {
        // Cap-sized payload delivered one byte at a time must fully fill.
        let data: Vec<u8> = (0..IMAGE_TYPE_SNIFF_BYTES).map(|i| (i % 251) as u8).collect();
        let mut r = Chunked { data: &data, pos: 0, chunk: 3 };
        let mut buf = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
        let n = read_to_cap(&mut r, &mut buf).unwrap();
        assert_eq!(n, IMAGE_TYPE_SNIFF_BYTES);
        assert_eq!(buf, data);
    }
}