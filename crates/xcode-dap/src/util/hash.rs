//! Deterministic hashing shared by the CLI (cache-file naming, setup's
//! marker blocks).

/// FNV-1a 64-bit (deterministic across processes, unlike `DefaultHasher`).
/// Used to derive stable cache file names from workspace / build-root paths,
/// and to hash the text of setup's marker blocks.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// [`fnv1a64`] as 16 lowercase hex digits, the form written into setup's
/// marker lines (`h=<hex>`).
pub fn fnv1a64_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a64(bytes))
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
        // Published FNV-1a 64 test vectors: marker hashes live in users'
        // config files, so the function must never drift.
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn hex_is_sixteen_lowercase_digits() {
        assert_eq!(fnv1a64_hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64_hex(b""), "cbf29ce484222325");
    }
}
