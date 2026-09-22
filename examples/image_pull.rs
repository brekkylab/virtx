//! Pull an OCI image into the local cache and print what came back.
//!
//!   cargo run --example image_pull -- alpine:3.21
//!
//! Set CORTEX_IMAGE_CACHE_ROOT to keep the cache somewhere other than the default.

use cortex_krun::image::{Cache, Registry};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reference = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "alpine:3.21".into());

    let cache = Cache::new();
    let registry = Registry::new();
    println!("cache: {}", cache.root().display());

    let started = std::time::Instant::now();
    let image = registry.pull(&reference, &cache).await?;

    println!(
        "{reference} -> {} ({:?})",
        image.manifest,
        started.elapsed()
    );
    println!("  platform:   {}", image.platform);
    println!(
        "  entrypoint: {:?} {:?}",
        image.config.entrypoint, image.config.cmd
    );
    println!("  workdir:    {:?}", image.config.workdir);
    for layer in &image.layers {
        println!("  layer {}", layer.digest);
    }
    Ok(())
}
