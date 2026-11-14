//! Wiping secrets from memory when they are dropped (T12.46).
//!
//! The crate forbids `unsafe`, so the wipe is not a volatile write: it overwrites the
//! whole allocation (spare capacity included) with zeros and then hands the buffer to
//! [`std::hint::black_box`] before freeing it, which keeps the compiler from removing the
//! writes as dead stores. Copies made outside our types (reqwest's `HeaderValue`, the TLS
//! buffers) are out of reach; secret header values are marked sensitive for hyper.

use std::sync::atomic::{Ordering, compiler_fence};

/// Overwrites every byte of `bytes`' allocation with zeros; afterwards its length is its
/// capacity, so the zeros cover spare capacity too, and no reallocation happens.
fn zero_allocation(bytes: &mut Vec<u8>) {
    let capacity = bytes.capacity();
    bytes.clear();
    bytes.resize(capacity, 0);
    std::hint::black_box(bytes.as_mut_slice());
    compiler_fence(Ordering::SeqCst);
}

/// Empties `secret`, zeroing its whole allocation before it is freed.
pub fn wipe_string(secret: &mut String) {
    let mut bytes = std::mem::take(secret).into_bytes();
    zero_allocation(&mut bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wipe_zeroes_the_whole_allocation_spare_capacity_included() {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(b"sk-invented-secret");
        let before = bytes.as_ptr();
        zero_allocation(&mut bytes);
        assert_eq!(bytes.as_ptr(), before, "no reallocation");
        assert!(bytes.len() >= 64);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn wipe_string_leaves_it_empty() {
        let mut s = String::from("sk-invented-secret");
        wipe_string(&mut s);
        assert!(s.is_empty());
    }
}
