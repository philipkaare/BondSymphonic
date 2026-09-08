use rand::RngCore;

/// `prefix` + 8 lowercase hex chars, e.g. `ws_3fa9c1d2`.
pub fn new_id(prefix: &str) -> String {
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}{}", hex::encode(b))
}
