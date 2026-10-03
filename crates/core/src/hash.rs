use std::io::Read;
use std::path::Path;

use md5::Md5;
use sha2::{Digest, Sha256};

use crate::Result;

/// SHA-256 and MD5 of a file in one pass. MD5 is only used because the Nexus
/// API identifies files by it; SHA-256 is what we trust locally.
pub fn file_digests(path: &Path) -> Result<(String, String)> {
    let mut f = std::fs::File::open(path)?;
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        md5.update(&buf[..n]);
    }
    Ok((hex::encode(sha.finalize()), hex::encode(md5.finalize())))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    Ok(file_digests(path)?.0)
}
