//! # nszlib
//!
//! The AOLserver zlib module (`nszlib/nszlib.c`): the `ns_zlib` command.
//!
//! * `ns_zlib compress data` — a zlib stream followed by the CRC-32 and the
//!   uncompressed length, both big-endian (`Ns_ZlibCompress`);
//! * `ns_zlib uncompress data` — the inverse, verifying the CRC;
//! * `ns_zlib gzip data` — gzip format;
//! * `ns_zlib gunzip file` — read and decompress a `.gz` file;
//! * `ns_zlib gzipfile file` — compress `file` to `file.gz` and remove it.
//!
//! Binary data crosses the script boundary as one char per byte.

use std::io::{Read, Write};

use flate2::read::{GzDecoder, ZlibDecoder};
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

/// `Ns_ZlibUncompress`: verify the trailer and inflate.
pub fn uncompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 8 {
        return Err("data too short".into());
    }
    let (stream, trailer) = data.split_at(data.len() - 8);
    let want_crc = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let want_len = u32::from_be_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;
    let mut out = Vec::with_capacity(want_len);
    ZlibDecoder::new(stream)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    let mut crc = Crc::new();
    crc.update(&out);
    if crc.sum() != want_crc || out.len() != want_len {
        return Err("crc mismatch".into());
    }
    Ok(out)
}

/// gzip at level 3 (`ZlibGzip`).
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut e = GzEncoder::new(Vec::new(), Compression::new(3));
    e.write_all(data)?;
    e.finish()
}

fn text_or_binary(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| bytes_to_binary(e.as_bytes()))
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
            .map(text_or_binary)
            .map_err(|_| Exception::error("nszlib: uncompress failed")),
        "gzip" => gzip(&binary_to_bytes(arg))
            .map(|b| bytes_to_binary(&b))
            .map_err(|_| Exception::error("gzip failed")),
        "gunzip" => {
            let f = std::fs::File::open(arg)
                .map_err(|_| Exception::error(format!("nszlib: gunzip: cannot open {arg}")))?;
            let mut out = Vec::new();
            GzDecoder::new(f)
                .read_to_end(&mut out)
                .map_err(|e| Exception::error(format!("nszlib: gunzip: read error {e}")))?;
            Ok(text_or_binary(out))
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

/// Module init: adds `ns_zlib` to the server's interps.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nszlib must be loaded as a per-server module".into()))?;
    server.add_command("ns_zlib", command(|_, argv| ns_zlib(argv)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
