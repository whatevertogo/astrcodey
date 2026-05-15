//! FNV-1a 哈希工具。

pub fn fnv1a_hash_bytes(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn hex_fingerprint(data: &[u8]) -> String {
    format!("{:016x}", fnv1a_hash_bytes(data))
}
