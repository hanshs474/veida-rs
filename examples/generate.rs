//! cargo run --example generate -- "an isometric coffee shop, pastel palette"
fn main() {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: generate \"<prompt>\"");
        std::process::exit(2);
    });
    match veida::generate(&prompt, &veida::Options::default()) {
        Ok(img) => println!("{} (watermarked: {})", img.url, img.watermarked),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
