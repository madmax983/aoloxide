//! Traditional Unix DES `crypt(3)` (`nsd/crypt.c`, `Ns_Encrypt`).
//!
//! Used by `ns_crypt` and the nsperm module's password files.  This is a
//! direct port of the reference bit-array implementation; it is *not* a
//! modern password hash and exists only for compatibility.

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const PC1_C: [u8; 28] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36,
];
const PC1_D: [u8; 28] = [
    63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5,
    28, 20, 12, 4,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const PC2_C: [u8; 24] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2,
];
const PC2_D: [u8; 24] = [
    41, 52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const E_SEL: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

struct Schedule {
    ks: [[u8; 48]; 16],
    e: [u8; 48],
}

impl Schedule {
    fn new(key: &[u8; 66]) -> Self {
        let mut c = [0u8; 28];
        let mut d = [0u8; 28];
        for i in 0..28 {
            c[i] = key[usize::from(PC1_C[i]) - 1];
            d[i] = key[usize::from(PC1_D[i]) - 1];
        }
        let mut ks = [[0u8; 48]; 16];
        for (i, k) in ks.iter_mut().enumerate() {
            for _ in 0..SHIFTS[i] {
                c.rotate_left(1);
                d.rotate_left(1);
            }
            for j in 0..24 {
                k[j] = c[usize::from(PC2_C[j]) - 1];
                k[j + 24] = d[usize::from(PC2_D[j]) - 28 - 1];
            }
        }
        Self { ks, e: E_SEL }
    }

    fn encrypt(&self, block: &mut [u8; 66]) {
        let mut l = [0u8; 64];
        for j in 0..64 {
            l[j] = block[usize::from(IP[j]) - 1];
        }
        for ks in &self.ks {
            let mut temp_l = [0u8; 32];
            temp_l.copy_from_slice(&l[32..]);
            let mut pre_s = [0u8; 48];
            for j in 0..48 {
                pre_s[j] = l[32 + usize::from(self.e[j]) - 1] ^ ks[j];
            }
            let mut f = [0u8; 32];
            for j in 0..8 {
                let t = 6 * j;
                let idx = (usize::from(pre_s[t]) << 5)
                    + (usize::from(pre_s[t + 1]) << 3)
                    + (usize::from(pre_s[t + 2]) << 2)
                    + (usize::from(pre_s[t + 3]) << 1)
                    + usize::from(pre_s[t + 4])
                    + (usize::from(pre_s[t + 5]) << 4);
                let k = S[j][idx];
                let t = 4 * j;
                f[t] = (k >> 3) & 1;
                f[t + 1] = (k >> 2) & 1;
                f[t + 2] = (k >> 1) & 1;
                f[t + 3] = k & 1;
            }
            for j in 0..32 {
                l[32 + j] = l[j] ^ f[usize::from(P[j]) - 1];
            }
            l[..32].copy_from_slice(&temp_l);
        }
        let (left, right) = l.split_at_mut(32);
        left.swap_with_slice(right);
        for j in 0..64 {
            block[j] = l[usize::from(FP[j]) - 1];
        }
    }
}

fn salt_bits(c: u8) -> u8 {
    let mut c = i32::from(c);
    if c > i32::from(b'Z') {
        c -= 6;
    }
    if c > i32::from(b'9') {
        c -= 7;
    }
    (c - i32::from(b'.')) as u8
}

/// Encrypt `pw` with the two-character `salt`, returning the 13-character
/// result (`Ns_Encrypt`).  Only the first 8 characters of `pw` matter.
pub fn crypt(pw: &str, salt: &str) -> String {
    let pwb = pw.as_bytes();
    let saltb = salt.as_bytes();
    let mut block = [0u8; 66];
    let mut i = 0;
    for &c in pwb {
        if i >= 64 || c == 0 {
            break;
        }
        for j in 0..7 {
            block[i] = (c >> (6 - j)) & 1;
            i += 1;
        }
        i += 1;
    }
    let mut sched = Schedule::new(&block);
    block = [0u8; 66];
    let mut out = [0u8; 13];
    for idx in 0..2 {
        let c = saltb.get(idx).copied().unwrap_or(0);
        out[idx] = c;
        let bits = salt_bits(c);
        for j in 0..6 {
            if (bits >> j) & 1 == 1 {
                sched.e.swap(6 * idx + j, 6 * idx + j + 24);
            }
        }
    }
    for _ in 0..25 {
        sched.encrypt(&mut block);
    }
    for i in 0..11 {
        let mut c: u8 = 0;
        for j in 0..6 {
            c = (c << 1) | block[6 * i + j];
        }
        c += b'.';
        if c > b'9' {
            c += 7;
        }
        if c > b'Z' {
            c += 6;
        }
        out[i + 2] = c;
    }
    if out[1] == 0 {
        out[1] = out[0];
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::crypt;

    #[test]
    fn known_vectors() {
        // Classic glibc/BSD DES crypt outputs.
        assert_eq!(crypt("test", "aa"), "aaqPiZY5xR5l.");
        assert_eq!(crypt("password", "ab"), "abJnggxhB/yWI");
        assert_eq!(crypt("", "xx"), "xx95eIq3U1EUg");
    }

    #[test]
    fn only_eight_chars_matter() {
        assert_eq!(crypt("12345678", "zz"), crypt("123456789abc", "zz"));
        assert_ne!(crypt("1234567", "zz"), crypt("12345678", "zz"));
        assert_eq!(crypt("abc", "zz").len(), 13);
    }
}
