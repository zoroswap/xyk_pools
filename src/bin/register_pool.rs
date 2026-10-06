use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use dotenv::dotenv;
use miden_client::account::{Account, AccountId};
use miden_client::transaction::ForeignAccount;
use miden_client::{
    Felt, Word, rpc::domain::account::AccountStorageRequirements,
    transaction::TransactionRequestBuilder,
};

use xyk_pool::common::{instantiate_simple_client, try_import_account};
use xyk_pool::pool_ops::{
    compile_custom_tx_script, get_combined_pool_library, get_registry_library,
};
use xyk_pool::test_utils::resolve_endpoint;
use xyk_pool::utils::{
    decode_pool_assets_mapping_word, is_empty_registry_word, ordered_assets_registry_key,
    pool_id_registry_key, slot_name,
};

const REGISTRY_BECH32: &str = "mtst1apa23tv7uxcgg527q4gf9mjz8qw3690l";
const POOL_BECH32: &str = "mtst1ap78vesvufgefut7jwkgn8nnxsrfphrf";
const EXPECTED_POOL_CODE_HASH: &str =
    "0x99b622ee4ccfc3077036ce2934a3945f964ebdc3ae2a877ec3264795f097c256";

fn build_register_pool_tx_source(
    pool_id: &AccountId,
    token0_id: &AccountId,
    token1_id: &AccountId,
) -> String {
    format!(
        "use zoro::registry\n\
         use miden::core::sys\n\
         begin\n\
             push.{pool_pfx}.{pool_sfx}.{t0_pfx}.{t0_sfx}.{t1_pfx}.{t1_sfx}\n\
             call.registry::register_pool\n\
             exec.sys::truncate_stack\n\
         end",
        pool_pfx = pool_id.prefix().as_u64(),
        pool_sfx = pool_id.suffix().as_canonical_u64(),
        t0_pfx = token0_id.prefix().as_u64(),
        t0_sfx = token0_id.suffix().as_canonical_u64(),
        t1_pfx = token1_id.prefix().as_u64(),
        t1_sfx = token1_id.suffix().as_canonical_u64(),
    )
}

fn parse_account_id(bech32: &str) -> Result<AccountId> {
    let (_network, id) = AccountId::from_bech32(bech32)
        .with_context(|| format!("Invalid account bech32: {bech32}"))?;
    Ok(id)
}

async fn load_registry_account(
    clients: &mut xyk_pool::common::MidenClients,
    registry_id: AccountId,
) -> Result<Account> {
    if let Some(account) = clients.client.get_account(registry_id).await? {
        println!("Using registry from local client store");
        return Ok(account);
    }
    println!("Registry not in local store; importing from network");
    try_import_account(clients, registry_id).await
}

fn print_preflight(
    registry_id: &AccountId,
    pool_id: &AccountId,
    token0_id: &AccountId,
    token1_id: &AccountId,
    pool_commitment: Word,
    accepted: Word,
    existing_pool_hash: Word,
    existing_assets_pool: Word,
) {
    println!("\nPreflight summary");
    println!(
        "  Registry:      {} ({})",
        registry_id.to_hex(),
        REGISTRY_BECH32
    );
    println!("  Pool:          {} ({})", pool_id.to_hex(), POOL_BECH32);
    println!("  Token0:        {}", token0_id.to_hex());
    println!("  Token1:        {}", token1_id.to_hex());
    println!("  Pool code:     {}", pool_commitment.to_hex());
    println!("  Expected code: {EXPECTED_POOL_CODE_HASH}");
    println!("  Accepted flag: {accepted:?}");
    println!("  Existing pool mapping: {existing_pool_hash:?}");
    println!("  Existing assets mapping: {existing_assets_pool:?}");
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let (label, endpoint) = resolve_endpoint();
    let base_dir = std::env::var("REGISTER_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("tmp").join(&label).join("deploy_registry"));
    fs::create_dir_all(&base_dir)?;

    let keystore_path = base_dir.join("keystore");
    let store_path = base_dir.join("store.sqlite3");

    println!("Registering pool on network: {label}");
    println!("State directory: {}", base_dir.display());

    let mut clients = instantiate_simple_client(
        keystore_path.to_str().unwrap(),
        store_path.to_str().unwrap(),
        &endpoint,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to connect client: {e:?}"))?;

    let registry_id = parse_account_id(REGISTRY_BECH32)?;
    let pool_id = parse_account_id(POOL_BECH32)?;
    let expected_commitment = Word::try_from(EXPECTED_POOL_CODE_HASH)
        .with_context(|| format!("Invalid expected pool code hash: {EXPECTED_POOL_CODE_HASH}"))?;

    let registry = load_registry_account(&mut clients, registry_id)
        .await
        .context("Failed to load registry account")?;
    let pool = try_import_account(&mut clients, pool_id)
        .await
        .context("Failed to import pool account")?;

    ensure!(
        registry_id.is_public(),
        "Registry account must be public, got {:?}",
        registry_id.account_type()
    );
    ensure!(
        pool_id.is_public(),
        "Pool account must be public, got {:?}",
        pool_id.account_type()
    );

    let pool_commitment = pool.code().commitment();
    ensure!(
        pool_commitment == expected_commitment,
        "Pool code commitment mismatch: on-chain {} != expected {}",
        pool_commitment.to_hex(),
        expected_commitment.to_hex()
    );

    let assets_word = pool
        .storage()
        .get_map_item(
            &slot_name("zoro::lp_local::assets_mapping"),
            Word::default(),
        )
        .context("Failed to read pool assets_mapping")?;
    let (token0_id, token1_id) = decode_pool_assets_mapping_word(assets_word)
        .context("Failed to decode pool assets_mapping word")?;

    let pool_registry_word = pool
        .storage()
        .get_item(&slot_name("zoro::lp_local::registry_id"))
        .context("Failed to read pool registry_id slot")?;
    let pool_registry_id =
        AccountId::try_from_elements(pool_registry_word[0], pool_registry_word[1])
            .context("Failed to decode pool registry_id slot")?;
    ensure!(
        pool_registry_id == registry_id,
        "Pool registry_id slot ({}) does not match target registry ({})",
        pool_registry_id.to_hex(),
        registry_id.to_hex()
    );

    let registry_storage = registry.storage();
    let accepted = registry_storage.get_map_item(
        &slot_name("zoro::registry::accepted_code_hashes_mapping"),
        pool_commitment,
    )?;
    ensure!(
        accepted[0] == Felt::ONE,
        "Pool code hash is not accepted by registry: {accepted:?}"
    );

    let pool_key = pool_id_registry_key(&pool_id);
    let existing_pool_hash =
        registry_storage.get_map_item(&slot_name("zoro::registry::pools_mapping"), pool_key)?;
    ensure!(
        is_empty_registry_word(existing_pool_hash),
        "Pool id is already registered: {existing_pool_hash:?}"
    );

    let assets_key = ordered_assets_registry_key(&token0_id, &token1_id)?;
    let existing_assets_pool = registry_storage.get_map_item(
        &slot_name("zoro::registry::assets_to_pool_mapping"),
        assets_key,
    )?;
    ensure!(
        is_empty_registry_word(existing_assets_pool),
        "Asset pair is already mapped to a pool: {existing_assets_pool:?}"
    );

    print_preflight(
        &registry_id,
        &pool_id,
        &token0_id,
        &token1_id,
        pool_commitment,
        accepted,
        existing_pool_hash,
        existing_assets_pool,
    );

    clients
        .client
        .add_account(&pool, true)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to track pool locally: {e:?}"))?;

    let registry_library = get_registry_library()?;
    let pool_library = get_combined_pool_library()?;
    if let Some(local_root) =
        pool_library.get_procedure_root_by_path("zoro::xyk_pool::get_account_code_commitment")
    {
        println!("\nLocal get_account_code_commitment procedure root: {local_root:?}");
    }
    println!(
        "On-chain pool code commitment: {}",
        pool_commitment.to_hex()
    );
    println!(
        "Local build pool code commitment: {}",
        xyk_pool::utils::get_pool_account_code_commitment().to_hex()
    );
    for (idx, proc_root) in pool.code().procedures().iter().enumerate() {
        println!("  On-chain pool procedure[{idx}]: {proc_root:?}");
    }

    let tx_source = build_register_pool_tx_source(&pool_id, &token0_id, &token1_id);
    println!("\nRegister pool tx script:\n{tx_source}");
    let register_script = compile_custom_tx_script(&registry_library, &tx_source)?;

    println!("\nSubmitting register_pool transaction on registry...");
    let foreign = ForeignAccount::public(pool_id, AccountStorageRequirements::default())?;
    clients.client.sync_state().await?;
    let register_tx_id = clients
        .client
        .submit_new_transaction(
            registry.id(),
            TransactionRequestBuilder::new()
                .custom_script(register_script)
                .foreign_accounts([foreign])
                .build()?,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to register pool on registry: {e:?}"))?;
    println!("  Register transaction: {register_tx_id}");

    clients.client.sync_state().await?;

    let registry_after = load_registry_account(&mut clients, registry_id)
        .await
        .context("Failed to reload registry after registration")?;
    let stored_code_hash = registry_after
        .storage()
        .get_map_item(&slot_name("zoro::registry::pools_mapping"), pool_key)?;
    ensure!(
        stored_code_hash == pool_commitment,
        "pools_mapping mismatch: stored {} != expected {}",
        stored_code_hash.to_hex(),
        pool_commitment.to_hex()
    );

    let stored_pool_for_assets = registry_after.storage().get_map_item(
        &slot_name("zoro::registry::assets_to_pool_mapping"),
        assets_key,
    )?;
    ensure!(
        stored_pool_for_assets == pool_key,
        "assets_to_pool_mapping mismatch: stored {stored_pool_for_assets:?} != expected {pool_key:?}"
    );

    println!("\nRegistration verified successfully!");
    println!(
        "  pools_mapping[{pool_key:?}] = {}",
        stored_code_hash.to_hex()
    );
    println!("  assets_to_pool_mapping[{assets_key:?}] = {stored_pool_for_assets:?}");
    println!("  Register tx: {register_tx_id}");

    Ok(())
}
