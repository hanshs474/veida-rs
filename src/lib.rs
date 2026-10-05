//! Generate images from Rust with no API key and no account.
//!
//! This crate calls the anonymous tier of [Veida](https://veida.ai/?utm_source=crates&utm_medium=package),
//! which meters its free allowance against a client id the crate invents rather
//! than an account you register, so a program using it runs on a machine with
//! nothing configured.
//!
//! ```no_run
//! let img = veida::generate("a paper boat at sunrise", &veida::Options::default())?;
//! println!("{}", img.url);
//! # Ok::<(), veida::Error>(())
//! ```
//!
//! Measured against the running service: a signed-out caller is granted 4
//! credits and one image costs 4, so the crate mints a fresh client id per
//! call. A ceiling of 30 credits per IP per day sits above that, roughly seven
//! images from one machine. Free output is 1K and watermarked.

use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The service this crate talks to.
pub const BASE_URL: &str = "https://veida.ai";
/// Credit balance a signed-out caller starts with.
pub const ANON_GRANT: u32 = 4;
/// What one generated image costs, so the grant pays for exactly one.
pub const COST_TEXT_TO_IMAGE: u32 = 4;
/// Credits one machine may spend per day regardless of client ids.
pub const IP_DAILY_CEILING: u32 = 30;
/// The free text-to-image engine.
pub const MODEL: &str = "veida-image-v1";
/// Aspect ratios the free engine accepts.
pub const ASPECT_RATIOS: [&str; 5] = ["1:1", "16:9", "9:16", "4:3", "3:4"];

/// One finished picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// A permanent, cacheable CDN link.
    pub url: String,
    /// Whether a mark was drawn on it. Signing in on veida.ai removes it.
    pub watermarked: bool,
}

/// Tunes a single call. `Options::default()` is a square image with a four minute deadline.
#[derive(Debug, Clone)]
pub struct Options {
    /// One of [`ASPECT_RATIOS`].
    pub aspect_ratio: String,
    /// How often the job is polled.
    pub poll_every: Duration,
    /// Bounds the whole call.
    pub timeout: Duration,
    /// Override for tests or a proxy.
    pub base_url: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            aspect_ratio: "1:1".into(),
            poll_every: Duration::from_secs(4),
            timeout: Duration::from_secs(240),
            base_url: BASE_URL.into(),
        }
    }
}

/// Everything that can go wrong, split by what the caller should do next.
#[derive(Debug)]
pub enum Error {
    /// A bad argument; fix the call.
    Invalid(String),
    /// The free allowance is spent for this machine today. Wait, or sign in.
    Quota(String),
    /// The content filter or the model refused the prompt. Reword it.
    Rejected,
    /// The deadline passed while the job was still queued or running.
    Timeout,
    /// The service answered with an error message.
    Service(String),
    /// The network or the response body failed.
    Transport(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "veida: {m}"),
            Error::Quota(m) => write!(f, "veida: free allowance spent: {m} (sign in at https://veida.ai/pricing)"),
            Error::Rejected => write!(f, "veida: prompt refused by the content filter; reword it rather than retrying"),
            Error::Timeout => write!(f, "veida: deadline passed while the job was still queued"),
            Error::Service(m) => write!(f, "veida: {m}"),
            Error::Transport(m) => write!(f, "veida: transport: {m}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Value,
}

/// A fresh client id. The grant pays for one image, so a reused id walls on
/// the second call of a loop.
pub fn anon_id() -> String {
    let mut b = [0u8; 8];
    if getrandom::getrandom(&mut b).is_err() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("rs-{n}");
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("rs-{hex}")
}

/// Turn a prompt into an image.
///
/// Naming the light, the material and the composition moves the result far
/// more than adding adjectives.
pub fn generate(prompt: &str, opts: &Options) -> Result<Image, Error> {
    if prompt.trim().is_empty() {
        return Err(Error::Invalid("prompt is required".into()));
    }
    let ratio = if opts.aspect_ratio.is_empty() { "1:1" } else { opts.aspect_ratio.as_str() };
    if !ASPECT_RATIOS.contains(&ratio) {
        return Err(Error::Invalid(format!("aspect_ratio must be one of {}", ASPECT_RATIOS.join(", "))));
    }
    let started = Instant::now();
    let agent = ureq::AgentBuilder::new()
        .timeout(opts.timeout.min(Duration::from_secs(60)))
        .user_agent(concat!("veida-rs/", env!("CARGO_PKG_VERSION")))
        .build();
    let id = anon_id();

    let env = read(
        agent
            .post(&format!("{}/api/ai/generate", opts.base_url))
            .set("x-anon-id", &id)
            .send_json(json!({
                "provider": "kie", "mediaType": "image", "model": MODEL,
                "scene": "text-to-image", "prompt": prompt,
                "options": { "aspect_ratio": ratio }
            })),
    )?;
    let task = parse_submit(env.code, &env.message, &env.data)?;

    loop {
        if started.elapsed() >= opts.timeout {
            return Err(Error::Timeout);
        }
        sleep(opts.poll_every);
        let polled = agent
            .get(&format!(
                "{}/api/ai/anon-query?taskId={}&provider=kie&mediaType=image",
                opts.base_url, task
            ))
            .set("x-anon-id", &id)
            .call();
        // A dropped connection mid-queue is not a failed generation; keep polling.
        let env = match read(polled) {
            Ok(e) if e.code == 0 => e,
            _ => continue,
        };
        if let Some(img) = parse_poll(&env.data)? {
            return Ok(img);
        }
    }
}

fn read(resp: Result<ureq::Response, ureq::Error>) -> Result<Envelope, Error> {
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(Error::Transport(e.to_string())),
    };
    resp.into_json::<Envelope>().map_err(|e| Error::Transport(e.to_string()))
}

/// Interpret the submit response. The quota wall answers HTTP 200 with code 0,
/// so it can only be recognised by a field.
pub fn parse_submit(code: i64, message: &str, data: &Value) -> Result<String, Error> {
    if code != 0 {
        return Err(Error::Service(message.to_string()));
    }
    if data.get("wall").and_then(Value::as_bool).unwrap_or(false) {
        return match data.get("reason").and_then(Value::as_str).unwrap_or("") {
            "anon_ip_daily" => Err(Error::Quota(format!(
                "this machine has used its {IP_DAILY_CEILING} credits for today"
            ))),
            other => Err(Error::Quota(other.to_string())),
        };
    }
    match data.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(Error::Service("the service returned no task id".into())),
    }
}

/// Interpret one poll. Status is not monotonic (pending and processing
/// alternate), so only a terminal failure ends the wait early.
pub fn parse_poll(data: &Value) -> Result<Option<Image>, Error> {
    if let Some(url) = data.get("images").and_then(|v| v.get(0)).and_then(Value::as_str) {
        let watermarked = data
            .get("watermarked")
            .and_then(|v| v.get(0))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Ok(Some(Image { url: url.to_string(), watermarked }));
    }
    match data.get("status").and_then(Value::as_str).map(str::to_lowercase).as_deref() {
        Some("failed") | Some("error") => Err(Error::Rejected),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        assert_ne!(anon_id(), anon_id());
        assert!(anon_id().starts_with("rs-"));
    }

    #[test]
    fn validates_arguments() {
        let o = Options::default();
        assert!(matches!(generate("  ", &o), Err(Error::Invalid(_))));
        let bad = Options { aspect_ratio: "2:1".into(), ..Default::default() };
        assert!(matches!(generate("x", &bad), Err(Error::Invalid(_))));
    }

    #[test]
    fn wall_is_a_200_recognised_by_field() {
        let d = json!({"wall": true, "reason": "anon_ip_daily"});
        assert!(matches!(parse_submit(0, "ok", &d), Err(Error::Quota(_))));
        let d = json!({"wall": true, "reason": "anon_credits"});
        assert!(matches!(parse_submit(0, "ok", &d), Err(Error::Quota(_))));
    }

    #[test]
    fn task_id_and_poll() {
        assert_eq!(parse_submit(0, "ok", &json!({"id": "t1"})).unwrap(), "t1");
        assert!(parse_poll(&json!({"status": "processing"})).unwrap().is_none());
        assert!(matches!(parse_poll(&json!({"status": "FAILED"})), Err(Error::Rejected)));
        let img = parse_poll(&json!({"images": ["https://cdn/x.webp"], "watermarked": [true]})).unwrap().unwrap();
        assert!(img.watermarked);
    }
}
