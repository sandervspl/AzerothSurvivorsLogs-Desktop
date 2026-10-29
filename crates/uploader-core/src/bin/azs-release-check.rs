//! Read-only release diagnostic: uses exactly the app's trust and delivery policy.
use anyhow::{Context, Result, ensure};
use azs_uploader_core::distribution::{ReleaseConfig, get_bounded};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let state = std::env::args()
        .nth(1)
        .context("Pass a dedicated verification state directory")?;
    let config = ReleaseConfig::configured()?;
    let verified = config.catalog(Path::new(&state)).await?;
    let catalog = verified.catalog;
    let mut evidence = Vec::new();
    let mut assets = vec![(
        catalog.addon.url.as_str(),
        catalog.addon.sha256.as_str(),
        catalog.addon.size,
    )];
    if let Some(desktop) = &catalog.desktop {
        ensure!(
            desktop.size > 0 && desktop.size <= 512 * 1024 * 1024,
            "Invalid desktop size"
        );
        assets.push((&desktop.url, &desktop.sha256, desktop.size));
    }
    for (url, expected, size) in assets {
        let bytes = get_bounded(&config.client()?, url, size).await?;
        let actual = azs_uploader_core::sha256(&bytes);
        ensure!(
            bytes.len() as u64 == size && actual == expected,
            "Release package hash/size mismatch"
        );
        evidence.push(serde_json::json!({"url":url,"sha256":actual,"size":size}));
    }
    println!(
        "{}",
        serde_json::json!({"sequence":catalog.sequence,"verifiedAssets":evidence})
    );
    Ok(())
}
