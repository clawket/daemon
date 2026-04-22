use std::sync::Mutex;

const ENCODING: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TIME_LEN: usize = 10;
const RANDOM_LEN: usize = 16;

struct State {
    last_time: u64,
    last_random: [u8; RANDOM_LEN],
}

static STATE: Mutex<State> = Mutex::new(State {
    last_time: 0,
    last_random: [0; RANDOM_LEN],
});

fn encode_time(mut t: u64, out: &mut [u8; TIME_LEN]) {
    for i in (0..TIME_LEN).rev() {
        out[i] = ENCODING[(t % 32) as usize];
        t /= 32;
    }
}

fn random_chars(out: &mut [u8; RANDOM_LEN]) {
    use rand::RngCore;
    let mut buf = [0u8; RANDOM_LEN];
    rand::thread_rng().fill_bytes(&mut buf);
    for i in 0..RANDOM_LEN {
        out[i] = ENCODING[(buf[i] as usize) & 31];
    }
}

fn increment_random(chars: &mut [u8; RANDOM_LEN]) -> bool {
    for i in (0..RANDOM_LEN).rev() {
        let idx = ENCODING.iter().position(|&c| c == chars[i]).unwrap_or(0);
        if idx < 31 {
            chars[i] = ENCODING[idx + 1];
            return true;
        }
        chars[i] = ENCODING[0];
    }
    false
}

pub fn ulid() -> String {
    let t = now_ms() as u64;
    let mut state = STATE.lock().unwrap();

    let mut random = state.last_random;
    if t == state.last_time && state.last_time != 0 {
        if !increment_random(&mut random) {
            random = [0; RANDOM_LEN];
            random_chars(&mut random);
        }
    } else {
        random_chars(&mut random);
        state.last_time = t;
    }
    state.last_random = random;
    drop(state);

    let mut time_chars = [0u8; TIME_LEN];
    encode_time(t, &mut time_chars);

    let mut out = String::with_capacity(TIME_LEN + RANDOM_LEN);
    out.push_str(std::str::from_utf8(&time_chars).unwrap());
    out.push_str(std::str::from_utf8(&random).unwrap());
    out
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", ulid())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn slugify(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_dash = true;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(40).collect()
}
