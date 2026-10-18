//! The second parity (Q) of dual parity spaces.
//!
//! Q is a Reed-Solomon code over GF(16) (polynomial x^4 + x + 1) in bit-matrix
//! form: every chunk of a unit, a logical sector of the space, is four
//! packets of a quarter of it (128 bytes of 512-byte sectors, 1024 bytes of
//! 4096-byte ones), packet `j` standing for bit `j` of 4-bit symbols.
//! Multiplying a chunk by a field element `e` XORs input packet `j` into
//! output packet `i` whenever bit `i` of `e * x^j` is set. Q = sum of
//! `coefficient(k) * D_k` over the data units. (**Verified**: the Q units
//! of `dual7`, 512-byte sectors, and `dual7k4`, 4096-byte sectors; 1.2.2
//! took chunks of 512 bytes on both, and rebuilt wrong data on the second:
//! audit 2026-10-16, M13.)

const POLY: u8 = 0x13;

/// Q coefficients of the data units, by number of data columns, as
/// measured on Windows-created spaces of 7 to 10 columns (the widths that use
/// a single group; wider dual parity spaces use a local reconstruction code,
/// see `Layout::parity_code`).
/// From 6 data columns on they are prefixes of one sequence; 5 data columns
/// use every second element of it.
pub fn coefficients(data_columns: u64) -> Option<&'static [u8]> {
    const WIDE: [u8; 8] = [13, 9, 4, 1, 12, 8, 5, 2];
    match data_columns {
        5 => Some(&[9, 1, 8, 2, 11]),
        6..=8 => Some(&WIDE[..data_columns as usize]),
        _ => None,
    }
}

fn mul_x(a: u8) -> u8 {
    let a = a << 1;
    if a & 0x10 != 0 { a ^ POLY } else { a }
}

pub fn mul(a: u8, b: u8) -> u8 {
    let (mut a, mut b, mut r) = (a, b, 0);
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = mul_x(a);
        b >>= 1;
    }
    r
}

pub fn inv(a: u8) -> Option<u8> {
    (1..16).find(|&b| mul(a, b) == 1)
}

/// `dst ^= e * src`; both are whole chunks of `chunk` bytes (the space's
/// logical sector size, 512 or 4096).
pub fn mul_region_xor(e: u8, src: &[u8], dst: &mut [u8], chunk: usize) {
    assert!(matches!(chunk, 512 | 4096) && src.len() == dst.len() && src.len().is_multiple_of(chunk));
    let packet = chunk / 4;
    // Column j of the bit matrix is e * x^j.
    let mut columns = [0u8; 4];
    let mut v = e;
    for c in &mut columns {
        *c = v;
        v = mul_x(v);
    }
    for (s, d) in src.chunks_exact(chunk).zip(dst.chunks_exact_mut(chunk)) {
        for (j, &col) in columns.iter().enumerate() {
            let input = &s[j * packet..(j + 1) * packet];
            for i in 0..4 {
                if col >> i & 1 != 0 {
                    d[i * packet..(i + 1) * packet]
                        .iter_mut()
                        .zip(input)
                        .for_each(|(o, x)| *o ^= x);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_arithmetic() {
        for a in 1..16 {
            assert_eq!(mul(a, inv(a).unwrap()), 1);
        }
        assert_eq!(mul(2, 8), 3); // x * x^3 = x^4 = x + 1
    }

    #[test]
    fn matches_windows_impulses() {
        // A single byte in packet 0 of data unit k lands in the packets set
        // in coefficient(k), as observed on a Windows-created pool.
        let expect: [&[usize]; 5] = [&[0, 0x180], &[0], &[0x180], &[0x80], &[0, 0x80, 0x180]];
        for (k, &e) in coefficients(5).unwrap().iter().enumerate() {
            let mut src = vec![0u8; 512];
            src[0] = 1;
            let mut q = vec![0u8; 512];
            mul_region_xor(e, &src, &mut q, 512);
            let set: Vec<usize> = (0..512).filter(|&i| q[i] != 0).collect();
            assert_eq!(set, expect[k], "data unit {k}");
        }
    }
}
