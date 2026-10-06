use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use dotenv::dotenv;
use miden_client::{Word, keystore::FilesystemKeyStore};
use serde::Deserialize;

use xyk_pool::common::{deploy_registry, instantiate_simple_client};
use xyk_pool::test_utils::resolve_endpoint;

#[derive(Deserialize)]
struct RegistryConfig {
    accepted_pool_code_hashes: Vec<String>,
}

fn load_registry_config() -> Result<Vec<Word>> {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("registry.toml");
    let config_source = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let config: RegistryConfig = toml::from_str(&config_source)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    ensure!(
        !config.accepted_pool_code_hashes.is_empty(),
        "{} must contain at least one accepted pool code hash",
        config_path.display()
    );

    config
        .accepted_pool_code_hashes
        .iter()
        .map(|hash| {
            Word::try_from(hash.as_str())
                .map_err(|err| anyhow::anyhow!("Invalid pool code hash {hash:?}: {err}"))
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let (label, endpoint) = resolve_endpoint();
    let base_dir = PathBuf::from("tmp").join(&label).join("deploy_registry");
    fs::create_dir_all(&base_dir)?;

    let keystore_path = base_dir.join("keystore");
    let store_path = base_dir.join("store.sqlite3");

    println!("Deploying registry to network: {label}");
    println!("State directory: {}", base_dir.display());

    let mut clients = instantiate_simple_client(
        keystore_path.to_str().unwrap(),
        store_path.to_str().unwrap(),
        &endpoint,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to connect client: {e:?}"))?;

    let keystore = FilesystemKeyStore::new(keystore_path)?;

    let accepted_pool_code_hashes = load_registry_config()?;
    println!(
        "Accepted pool code hashes loaded: {}",
        accepted_pool_code_hashes.len()
    );
    for code_hash in &accepted_pool_code_hashes {
        println!("  {} ({code_hash:?})", code_hash.to_hex());
    }

    let (registry, _key_pair) =
        deploy_registry(&mut clients.client, keystore, &accepted_pool_code_hashes)
            .await
            .map_err(|e| anyhow::anyhow!("Registry deployment failed: {e:?}"))?;

    let network_id = endpoint.to_network_id();
    println!("\nRegistry deployed successfully!");
    println!("  ID (hex):    {}", registry.id().to_hex());
    println!("  ID (bech32): {}", registry.id().to_bech32(network_id));
    println!(
        "  Accepted pool code hashes: {}",
        accepted_pool_code_hashes.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::load_registry_config;

    #[test]
    fn registry_config_contains_valid_pool_code_hashes() {
        let hashes = load_registry_config().expect("registry config should be valid");

        assert!(!hashes.is_empty());
        assert!(hashes.iter().all(|hash| hash.to_hex().starts_with("0x")));
    }
}
