//! VNC Authentication (security type 2) challenge/response.

use des::Des;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt, KeyInit};

/// Length of the server challenge and of the client response.
pub(super) const CHALLENGE_LEN: usize = 16;

/// Computes the response to a VNC Authentication challenge.
///
/// The password is truncated or zero-padded to eight bytes and used as a DES key with the bit
/// order of every byte reversed (a quirk inherited from the original VNC implementation). Both
/// 8-byte halves of the challenge are encrypted independently (ECB).
pub(super) fn vnc_auth_response(
    password: &str,
    challenge: &[u8; CHALLENGE_LEN],
) -> [u8; CHALLENGE_LEN] {
    let mut key = [0u8; 8];
    for (key_byte, password_byte) in key.iter_mut().zip(password.as_bytes()) {
        *key_byte = password_byte.reverse_bits();
    }

    let cipher = Des::new(&GenericArray::from(key));
    let mut response = *challenge;
    for block in response.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    response
}
