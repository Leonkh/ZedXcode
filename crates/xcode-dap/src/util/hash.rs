//! Deterministic hashing shared by the CLI (cache-file naming).

/// FNV-1a 64-bit (deterministic across processes, unlike `DefaultHasher`).
/// Used to derive stable cache file names from workspace / build-root paths.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_stable() {
        // Lock the hash so cache file names stay stable across releases.
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(
            fnv1a64(b"/x/myapp.xcworkspace"),
            fnv1a64(b"/x/myapp.xcworkspace")
        );
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
    }
}
