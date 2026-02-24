use rand::Rng;

pub const CHARSET_ALPHANUM_UNAMBIGUOUS: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Generates a random device code of a given length using unambiguous alphanumeric characters.
pub fn generate_device_code(length: usize) -> String {
    let mut rng = rand::rng();
    (0..length)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET_ALPHANUM_UNAMBIGUOUS.len());
            CHARSET_ALPHANUM_UNAMBIGUOUS[idx] as char
        })
        .collect()
}
