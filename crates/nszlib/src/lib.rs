//! # nszlib
//!
//! The AOLserver zlib module (`nszlib/nszlib.c`): the `ns_zlib` command.
//!
//! * `ns_zlib compress data` — a zlib stream followed by the CRC-32 and the
//!   uncompressed length, both big-endian (`Ns_ZlibCompress`);
//! * `ns_zlib uncompress data` — the inverse, verifying the CRC;
//! * `ns_zlib gzip data` — gzip format;
//! * `ns_zlib gunzip file` — read and decompress a `.gz` file (all
//!   members; a file that is not gzip is returned as-is, like `gzread`);
//! * `ns_zlib gzipfile file` — compress `file` to `file.gz` and remove it.
//!
//! Loading the module also installs the response compression procedure
//! (`Ns_SetGzipProc`), without which the server never gzips output.
//!
//! Binary data crosses the script boundary as one char per byte; the
//! results of `uncompress` and `gunzip` are strings decoded leniently, as
//! Tcl does (see [`tcl_string`]).

use std::io::{Read, Write};

use flate2::bufread::GzDecoder as GzMemberDecoder;
use flate2::read::ZlibDecoder;
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::{Compression, Crc};
use ns_script::{binary_to_bytes, bytes_to_binary, command, wrong_args, Exception};
use nsd::{ModuleContext, NsError};

/// `Ns_ZlibCompress`: zlib stream + CRC-32 + length (big-endian).
pub fn compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(3));
    e.write_all(data)?;
    let mut out = e.finish()?;
    let mut crc = Crc::new();
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    Ok(out)
}

/// Inflate a zlib `stream`, reading at most `limit + 1` bytes of output so
/// a small input cannot expand without bound.
pub fn inflate_limited(stream: &[u8], limit: usize) -> std::io::Result<Vec<u8>> {
    // Reserve what the input could plausibly expand to, not the declared
    // size (which may be up to 4 GB).
    let mut out = Vec::with_capacity(limit.min(stream.len().saturating_mul(4)));
    ZlibDecoder::new(stream)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    Ok(out)
}

/// `Ns_ZlibUncompress`: inflate into a buffer of the length declared in
/// the trailer and verify the CRC.  As with zlib's `uncompress`, output
/// longer than declared is an error (`Z_BUF_ERROR`); shorter output is
/// accepted if its CRC matches.
pub fn uncompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 8 {
        return Err("data too short".into());
    }
    let (stream, trailer) = data.split_at(data.len() - 8);
    let want_crc = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let want_len = u32::from_be_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;
    let out = inflate_limited(stream, want_len).map_err(|e| e.to_string())?;
    if out.len() > want_len {
        return Err("buffer error".into());
    }
    let mut crc = Crc::new();
    crc.update(&out);
    if crc.sum() != want_crc {
        return Err("crc mismatch".into());
    }
    Ok(out)
}

/// Decompress a gzip file's contents the way zlib's `gzread` does: every
/// concatenated member is decoded, trailing data that is not a gzip
/// header is ignored, and data that does not start with the gzip magic is
/// returned unchanged (transparent mode).
pub fn gunzip_bytes(data: &[u8]) -> std::io::Result<Vec<u8>> {
    const MAGIC: [u8; 2] = [0x1f, 0x8b];
    if !data.starts_with(&MAGIC) {
        return Ok(data.to_vec());
    }
    let mut out = Vec::new();
    let mut rest = data;
    while rest.starts_with(&MAGIC) {
        let mut d = GzMemberDecoder::new(rest);
        d.read_to_end(&mut out)?;
        rest = d.into_inner();
    }
    Ok(out)
}

/// gzip at `level` (`ZlibGzip`); installed as the server's
/// [`nsd::global::GzipProc`].
pub fn gzip_level(data: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    let mut e = GzEncoder::new(
        Vec::with_capacity(data.len() / 2),
        Compression::new(level.min(9)),
    );
    e.write_all(data)?;
    e.finish()
}

/// gzip at level 3 (`ns_zlib gzip`).
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    gzip_level(data, 3)
}

/// Bytes to a script string as Tcl 8.6 reads them (`Tcl_NewStringObj`
/// then `Tcl_UtfToUniChar`): valid UTF-8 sequences are decoded, the
/// modified-UTF-8 NUL `C0 80` is U+0000, and every other byte that does
/// not start a valid sequence is its Latin-1 character.
pub fn tcl_string(b: Vec<u8>) -> String {
    let b = match String::from_utf8(b) {
        Ok(s) => return s,
        Err(e) => e.into_bytes(),
    };
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0xC0 && b.get(i + 1) == Some(&0x80) {
            out.push('\0');
            i += 2;
            continue;
        }
        let n = match b[i] {
            0..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        if let Some(s) = b
            .get(i..i + n)
            .filter(|_| n > 0)
            .and_then(|s| std::str::from_utf8(s).ok())
        {
            out.push_str(s);
            i += n;
        } else {
            out.push(char::from(b[i]));
            i += 1;
        }
    }
    out
}

fn ns_zlib(argv: &[String]) -> Result<String, Exception> {
    let (_, opt) = nsd::cmds::subcommand(
        argv,
        &["compress", "uncompress", "gzip", "gunzip", "gzipfile"],
    )?;
    let arg = argv.get(2).ok_or_else(|| wrong_args(argv, 2, "data"))?;
    match opt {
        "compress" => compress(&binary_to_bytes(arg))
            .map(|b| bytes_to_binary(&b))
            .map_err(|_| Exception::error("nszlib: compress failed")),
        "uncompress" => uncompress(&binary_to_bytes(arg))
            .map(tcl_string)
            .map_err(|_| Exception::error("nszlib: uncompress failed")),
        "gzip" => gzip(&binary_to_bytes(arg))
            .map(|b| bytes_to_binary(&b))
            .map_err(|_| Exception::error("gzip failed")),
        "gunzip" => {
            let data = std::fs::read(arg)
                .map_err(|_| Exception::error(format!("nszlib: gunzip: cannot open {arg}")))?;
            let out = gunzip_bytes(&data)
                .map_err(|e| Exception::error(format!("nszlib: gunzip: read error {e}")))?;
            Ok(tcl_string(out))
        }
        _ => {
            let data = std::fs::read(arg)
                .map_err(|_| Exception::error(format!("nszlib: gzipfile: cannot open {arg}")))?;
            let out = format!("{arg}.gz");
            let z = gzip(&data)
                .map_err(|e| Exception::error(format!("nszlib: gzipfile: write error {e}")))?;
            std::fs::write(&out, z)
                .map_err(|_| Exception::error(format!("nszlib: gzipfile: cannot create {out}")))?;
            let _ = std::fs::remove_file(arg);
            Ok(out)
        }
    }
}

/// Module init (`NsZlibModInit`): installs the gzip procedure for
/// response compression and adds `ns_zlib` to the server's interps.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    ctx.global.set_gzip_proc(gzip_level);
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nszlib must be loaded as a per-server module".into()))?;
    server.add_command("ns_zlib", command(|_, argv| ns_zlib(argv)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn compress_roundtrip_and_trailer() {
        let data = b"hello hello hello world".repeat(5);
        let c = compress(&data).expect("compress");
        let n = c.len();
        assert_eq!(
            u32::from_be_bytes([c[n - 4], c[n - 3], c[n - 2], c[n - 1]]) as usize,
            data.len()
        );
        assert_eq!(uncompress(&c).expect("uncompress"), data);
        let mut bad = c;
        bad[n - 8] ^= 0xff;
        assert!(uncompress(&bad).is_err());
        assert!(uncompress(b"x").is_err());
    }

    fn with_trailer(stream: &[u8], crc_of: &[u8], len: u32) -> Vec<u8> {
        let mut crc = Crc::new();
        crc.update(crc_of);
        let mut v = stream.to_vec();
        v.extend_from_slice(&crc.sum().to_be_bytes());
        v.extend_from_slice(&len.to_be_bytes());
        v
    }

    fn zlib_stream(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::new(9));
        e.write_all(data).expect("w");
        e.finish().expect("finish")
    }

    #[test]
    fn uncompress_is_bounded_by_declared_length() {
        // 64 MB of zeros declared as 16 bytes: zlib's uncompress() fails
        // with Z_BUF_ERROR as soon as output passes the declared length.
        let bomb = zlib_stream(&vec![0u8; 64 << 20]);
        assert!(uncompress(&with_trailer(&bomb, &[0; 16], 16)).is_err());
        let out = inflate_limited(&bomb, 16).expect("inflate");
        assert_eq!(out.len(), 17, "stops one byte past the limit");
        // Output shorter than declared is accepted when the CRC matches
        // (uncompress returns Z_OK at stream end).
        let short = with_trailer(&zlib_stream(b"abc"), b"abc", 10);
        assert_eq!(uncompress(&short).expect("short"), b"abc");
    }

    #[test]
    fn gunzip_multi_member_and_transparent() {
        let d = tempfile::tempdir().expect("tmp");
        let multi = d.path().join("m.gz");
        let mut data = gzip(b"first ").expect("g1");
        data.extend(gzip(b"second").expect("g2"));
        let mut with_garbage = data.clone();
        std::fs::write(&multi, &data).expect("w");
        let p = multi.to_string_lossy().into_owned();
        assert_eq!(
            ns_zlib(&sv(&["ns_zlib", "gunzip", &p])).expect("multi"),
            "first second"
        );
        // Trailing garbage after a member is ignored (gz_look).
        with_garbage.extend_from_slice(b"junk");
        std::fs::write(&multi, &with_garbage).expect("w");
        assert_eq!(
            ns_zlib(&sv(&["ns_zlib", "gunzip", &p])).expect("garbage"),
            "first second"
        );
        // A file that is not gzip is read as-is (transparent mode).
        let plain = d.path().join("plain.txt");
        std::fs::write(&plain, "just text").expect("w");
        let p = plain.to_string_lossy().into_owned();
        assert_eq!(
            ns_zlib(&sv(&["ns_zlib", "gunzip", &p])).expect("plain"),
            "just text"
        );
        let empty = d.path().join("empty");
        std::fs::write(&empty, "").expect("w");
        let p = empty.to_string_lossy().into_owned();
        assert_eq!(ns_zlib(&sv(&["ns_zlib", "gunzip", &p])).expect("empty"), "");
        // A corrupt gzip member is a read error.
        let bad = d.path().join("bad.gz");
        std::fs::write(&bad, [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 0xff, 0xff]).expect("w");
        let p = bad.to_string_lossy().into_owned();
        assert!(ns_zlib(&sv(&["ns_zlib", "gunzip", &p])).is_err());
    }

    #[test]
    fn lenient_utf8_like_tcl() {
        // Valid sequences decode; each invalid byte is its Latin-1 char.
        assert_eq!(tcl_string(b"caf\xc3\xa9\xff".to_vec()), "caf\u{e9}\u{ff}");
        assert_eq!(tcl_string(b"\xe2\x82".to_vec()), "\u{e2}\u{82}");
        assert_eq!(
            tcl_string(b"a\xc0\x80b".to_vec()),
            "a\0b",
            "modified UTF-8 NUL"
        );
        assert_eq!(tcl_string("\u{1F600}x".as_bytes().to_vec()), "\u{1F600}x");
        let c = compress(b"caf\xc3\xa9\xff").expect("c");
        assert_eq!(
            ns_zlib(&["ns_zlib".into(), "uncompress".into(), bytes_to_binary(&c)]).expect("u"),
            "caf\u{e9}\u{ff}"
        );
    }

    #[test]
    fn gzip_proc_matches_level() {
        let data = b"abcabcabc".repeat(100);
        let z = gzip_level(&data, 6).expect("gz");
        let mut out = Vec::new();
        GzDecoder::new(&z[..])
            .read_to_end(&mut out)
            .expect("gunzip");
        assert_eq!(out, data);
    }

    #[test]
    fn commands() {
        let c = ns_zlib(&sv(&["ns_zlib", "compress", "abc"])).expect("c");
        assert_eq!(
            ns_zlib(&["ns_zlib".into(), "uncompress".into(), c]).expect("u"),
            "abc"
        );
        let g = ns_zlib(&sv(&["ns_zlib", "gzip", "zzz"])).expect("g");
        let mut out = String::new();
        GzDecoder::new(&binary_to_bytes(&g)[..])
            .read_to_string(&mut out)
            .expect("gunzip");
        assert_eq!(out, "zzz");
        let d = tempfile::tempdir().expect("tmp");
        let f = d.path().join("f.txt");
        std::fs::write(&f, "file data").expect("w");
        let p = f.to_string_lossy().into_owned();
        let gz = ns_zlib(&sv(&["ns_zlib", "gzipfile", &p])).expect("gzipfile");
        assert!(!f.exists());
        assert_eq!(
            ns_zlib(&sv(&["ns_zlib", "gunzip", &gz])).expect("gunzip"),
            "file data"
        );
        assert!(ns_zlib(&sv(&["ns_zlib", "gunzip", "/nonexistent"])).is_err());
        assert!(ns_zlib(&sv(&["ns_zlib", "uncompress", "junk"])).is_err());
        assert!(ns_zlib(&sv(&["ns_zlib", "compress"])).is_err());
    }
}
