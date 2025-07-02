//! Recursive variant of the NTRU Prime encoding for arbitrary bases.
//!
//! In contrast to the original encoding, this variant does not require any allocations.

/// Encode the given bytes into the given string using the given alphabet.
pub fn encode(out: &mut String, alphabet: &[char], limit: u32, bytes: &[u8]) {
    fn emit_digit(out: &mut String, alphabet: &[char], digit: u32, base: u32) -> (u32, u32) {
        let alphabet_base = alphabet.len() as u32;
        out.push(alphabet[(digit % alphabet_base) as usize]);
        (digit / alphabet_base, base.div_ceil(alphabet_base))
    }

    fn encode_rec(out: &mut String, alphabet: &[char], limit: u32, bytes: &[u8]) -> (u32, u32) {
        match bytes.len() {
            0 => (0, 0),
            1 => (bytes[0] as u32, 256),
            _ => {
                let mid = bytes.len() / 2;
                let (first_digit, first_base) = encode_rec(out, alphabet, limit, &bytes[..mid]);
                let (second_digit, second_base) = encode_rec(out, alphabet, limit, &bytes[mid..]);
                let mut base = first_base * second_base;
                let mut digit = first_digit + second_digit * first_base;
                while base >= limit {
                    (digit, base) = emit_digit(out, alphabet, digit, base);
                }
                (digit, base)
            }
        }
    }

    let (mut digit, mut base) = encode_rec(out, alphabet, limit, bytes);
    while base > 1 {
        (digit, base) = emit_digit(out, alphabet, digit, base);
    }
}

/// Decode the given string into the given byte slice using the given alphabet.
///
/// **🚨 This implementation is unfinished. 🚨**
///
/// Note that this requires the length of the bytes to be known in advance.
///
/// Unfortunately, in contrast to the original NTRU Prime encoding, decoding the variant
/// we are using here is more complicated. This implementation is unfinished. Within
/// Nexigon, there is no need for decoding, so this is not an issue.
#[expect(
    dead_code,
    unused_mut,
    unused_assignments,
    reason = "unfinished implementation"
)]
fn decode(s: &str, alphabet: &[char], limit: u32, bytes: &mut [u8]) {
    fn consume_digit<'s>(s: &'s str, alphabet: &[char], base: u32) -> (&'s str, char, u32) {
        let alphabet_base = alphabet.len() as u32;
        let mut digits = s.chars();
        let digit = digits.next().unwrap();
        (digits.as_str(), digit, base.div_ceil(alphabet_base))
    }

    fn decode_rec<'s>(s: &'s str, alphabet: &[char], limit: u32, bytes: &[u8]) -> (&'s str, u32) {
        match bytes.len() {
            0 => (s, 0),
            1 => (s, 256),
            _ => {
                let mid = bytes.len() / 2;
                let (s, first_base) = decode_rec(s, alphabet, limit, &bytes[..mid]);
                let (mut s, second_base) = decode_rec(s, alphabet, limit, &bytes[mid..]);
                let mut base = first_base * second_base;
                let mut digit;
                while base >= limit {
                    (s, digit, base) = consume_digit(s, alphabet, base);
                    // TODO: This lookup is not ideal.
                    let digit_value = alphabet.iter().position(|c| *c == digit).unwrap() as u32;
                    let _ = digit_value;
                    todo!("do something with the digit");
                }
                (s, base)
            }
        }
    }

    let (mut s, mut base) = decode_rec(s, alphabet, limit, bytes);
    let mut digit;
    while base > 1 {
        (s, digit, base) = consume_digit(s, alphabet, base);
        let digit_value = alphabet.iter().position(|c| *c == digit).unwrap() as u32;
        let _ = digit_value;
        todo!("do something with the digit");
    }
}
