use std::sync::Arc;
use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow};
use miden_client::account::component::BasicWallet;
use miden_client::account::{
    Account, AccountBuilder, AccountBuilderSchemaCommitmentExt, AccountId, AccountType, StorageMap,
    StorageSlot,
};
use miden_client::auth::{AuthSecretKey, NoAuth};
use miden_client::{
    ClientError, Felt, Word,
    asset::{AssetAmount, FungibleAsset, TokenSymbol},
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::{FilesystemKeyStore, Keystore},
    note::{Note, NoteError, NoteType},
    rpc::GrpcClient,
    store::TransactionFilter,
    transaction::{TransactionRequestBuilder, notes_from_output},
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::StorageMapKey;
use miden_standards::account::faucets::{FungibleFaucet, TokenName};
use miden_standards::account::policies::{
    BurnPolicyConfig, MintPolicyConfig, PolicyRegistration, TokenPolicyManager,
};
use rand::RngCore;
use tracing::{debug, info, warn};

use serde::{Deserialize, Serialize};

use crate::pool_ops::build_dummy_register_note;
use crate::{
    pool_ops::{
        create_library, get_combined_pool_library, get_lp_local_fuzz_dummy_library,
        get_lp_local_library, get_pool_library, get_registry_library, get_storage_utils_library,
        kernel_assembler, shared_source_manager,
    },
    utils::{
        auth_single_sig_component, fetch_vault_for_account_from_chain, get_register_note_root_hash,
        slot_name, zoro_component,
    },
};

use miden_client::{Client, note::NoteTag, rpc::Endpoint};
pub type MidenClient = Client<FilesystemKeyStore>;
pub struct MidenClients {
    pub client: MidenClient,
    pub rpc_api: Arc<GrpcClient>,
    pub endpoint: Endpoint,
}

pub async fn instantiate_simple_client(
    keystore_path: &str,
    store_path: &str,
    endpoint: &Endpoint,
) -> Result<MidenClients, ClientError> {
    let timeout_ms = 30_000;
    let rpc_api = Arc::new(GrpcClient::new(endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(keystore_path.into())
        .unwrap_or_else(|err| panic!("Failed to create keystore: {err:?}"))
        .into();
    println!("\nConnecting to endpoint: {}", endpoint);

    // Share our source manager with the client so that error codes raised by the
    // TransactionExecutor (and anywhere else inside the client) can be resolved back to the
    // original MASM source spans/messages, instead of only showing a plain numeric error code.
    // Without this, the client falls back to its own empty `DefaultSourceManager`, which has no
    // knowledge of the modules/libraries we assembled ourselves via `kernel_assembler()`.
    let mut client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .authenticator(keystore)
        .in_debug_mode(true.into())
        .sqlite_store(store_path.into())
        .source_manager(shared_source_manager())
        .build()
        .await?;

    println!("\nSyncing state...");
    let sync_summary = client.sync_state().await?;
    println!("\nLatest block: {}", sync_summary.block_num);

    Ok(MidenClients {
        client,
        rpc_api,
        endpoint: endpoint.clone(),
    })
}

/// Creates a basic regular account with updatable code.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore to store the account's authentication key
///
/// # Returns
/// Tuple of `(Account, AuthSecretKey)`
pub async fn create_basic_account(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(BasicWallet);
    let account = builder.build_with_schema_commitment()?;
    client.add_account(&account, false).await?;
    keystore.add_key(&key_pair, account.id()).await.unwrap();
    client.sync_state().await?;

    // dummy tx to get the new account into node
    touch_account(client, &account).await.unwrap();

    Ok((account, key_pair))
}

/// Deploys a constant-product pool account configured for the given token pair.
///
/// Storage slots:
///   - `reserve`:   [reserve0, reserve1, total_lp, 0]  (initially empty)
///   - `config`:    [token0_prefix, token0_suffix, token1_prefix, token1_suffix]
///   - `lp_shares`: StorageMap (initially empty)
pub async fn deploy_xyk_pool(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
    token0_id: &AccountId,
    token1_id: &AccountId,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let sync_summary = client.sync_state().await?;
    println!("\nLatest block: {}", sync_summary.block_num);
    println!("\n[STEP 1] Create xyk_pool account");

    // let pool_code = read_masm_to_string("accounts", "xyk_pool")
    //     .unwrap_or_else(|e| panic!("Failed to read xyk_pool code: {e:?}"));

    // let assembler = TransactionKernel::assembler(); //.with_warnings_as_errors(true);

    let reserves = StorageSlot::with_empty_value(slot_name("zoro::lp_local::reserve"));
    let pool_assets: Word = [
        token0_id.prefix().as_felt(),
        token0_id.suffix(),
        token1_id.prefix().as_felt(),
        token1_id.suffix(),
    ]
    .into();
    let assets_mapping =
        StorageSlot::with_value(slot_name("zoro::lp_local::assets_mapping"), pool_assets);

    // let xyk_pool_library = create_library(assembler.clone(), "zoro::xyk_pool", &pool_code)
    //     .map_err(|e| anyhow!("Failed to create pool library: {e:?}"))
    //     .unwrap();
    let xyk_pool_library = get_pool_library().unwrap();
    let xyk_pool_component = zoro_component(
        xyk_pool_library,
        vec![reserves, assets_mapping],
        "zoro::xyk_pool",
    )
    .map_err(|e| ClientError::AccountError(e))?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let xyk_pool_contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(xyk_pool_component.clone())
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(BasicWallet)
        .build_with_schema_commitment()?;

    println!(
        "pool contract commitment hash: {:?}",
        xyk_pool_contract.to_commitment().to_hex()
    );
    println!(
        "pool config: token0={}, token1={}",
        token0_id.to_hex(),
        token1_id.to_hex(),
    );

    keystore
        .add_key(&key_pair, xyk_pool_contract.id())
        .await
        .unwrap();
    client
        .add_account(&xyk_pool_contract.clone(), false)
        .await?;
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok((xyk_pool_contract, key_pair))
}

/// Deploys an lp_local pool account for the given token pair.
///
/// Storage slots (must match lp_local.masm):
///   - `reserve_mapping`: [reserve0, reserve1, 0, 0] (initially empty)
///   - `total_supply`: [total_lp, 0, 0, 0] (initially empty)
///   - `user_deposits_mapping`: map slot (initially empty)
pub async fn deploy_lp_local_pool(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
    token0_id: &AccountId,
    token1_id: &AccountId,
) -> Result<(Account, AuthSecretKey)> {
    let _ = (token0_id, token1_id);
    let lp_local_library = get_lp_local_library()
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;

    let mut assets_mapping = StorageMap::new();
    assets_mapping.insert(
        StorageMapKey::new(Word::default()),
        [
            token1_id.suffix(),
            token1_id.prefix().as_felt(),
            token0_id.suffix(),
            token0_id.prefix().as_felt(),
        ]
        .into(),
    )?;
    let assets_mapping_slot =
        StorageSlot::with_map(slot_name("zoro::lp_local::assets_mapping"), assets_mapping);
    let reserve_slot = StorageSlot::with_empty_value(slot_name("zoro::lp_local::reserve"));
    let total_supply_slot =
        StorageSlot::with_empty_value(slot_name("zoro::lp_local::total_supply"));
    let mut user_deposits_mapping = StorageMap::new();
    user_deposits_mapping.insert(
        StorageMapKey::new(Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE])),
        Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE]),
    )?;
    let user_deposits_slot = StorageSlot::with_map(
        slot_name("zoro::lp_local::user_deposits_mapping"),
        user_deposits_mapping,
    );
    let registry_id_slot = StorageSlot::with_empty_value(slot_name("zoro::lp_local::registry_id"));
    let register_note_root =
        StorageSlot::with_empty_value(slot_name("zoro::lp_local::register_note_root"));

    let lp_local_component = zoro_component(
        lp_local_library,
        vec![
            assets_mapping_slot,
            reserve_slot,
            total_supply_slot,
            user_deposits_slot,
            registry_id_slot,
            register_note_root,
        ],
        "zoro::lp_local",
    )
    .map_err(|e| ClientError::AccountError(e))?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let lp_local_contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(lp_local_component)
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .map_err(|e| anyhow!("Failed to build lp_local contract: {e:?}"))
        .unwrap();

    keystore
        .add_key(&key_pair, lp_local_contract.id())
        .await
        .map_err(|e| anyhow!("keystore: {e}"))?;
    client
        .add_account(&lp_local_contract.clone(), false)
        .await?;
    client.sync_state().await?;
    touch_account(client, &lp_local_contract).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok((lp_local_contract, key_pair))
}

/// Deploys a combined pool account with both `lp_local` and `xyk_pool` components.
///
/// The `lp_local` component provides LP management (deposit, withdraw, mint, burn).
/// The `xyk_pool` component provides swap logic and references lp_local storage.
///
/// Storage slots (from lp_local):
///   - `assets_mapping`: map with token0/token1 IDs
///   - `reserve`: [reserve0, reserve1, 0, 0]
///   - `total_supply`: [total_lp, 0, 0, 0]
///   - `user_deposits_mapping`: map slot
pub async fn deploy_combined_pool(
    client: &mut MidenClient,
    _keystore: FilesystemKeyStore,
    token0_id: &AccountId,
    token1_id: &AccountId,
    registry_id: &AccountId,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let lp_local_library = get_lp_local_library()
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;
    let xyk_pool_library = get_combined_pool_library()
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;

    // lp_local storage slots
    let mut assets_mapping = StorageMap::new();
    assets_mapping.insert(
        StorageMapKey::new(Word::default()),
        [
            token1_id.suffix(),
            token1_id.prefix().as_felt(),
            token0_id.suffix(),
            token0_id.prefix().as_felt(),
        ]
        .into(),
    )?;
    let assets_mapping_slot =
        StorageSlot::with_map(slot_name("zoro::lp_local::assets_mapping"), assets_mapping);
    let reserve_slot = StorageSlot::with_empty_value(slot_name("zoro::lp_local::reserve"));
    let total_supply_slot =
        StorageSlot::with_empty_value(slot_name("zoro::lp_local::total_supply"));
    let registry_id_slot = StorageSlot::with_value(
        slot_name("zoro::lp_local::registry_id"),
        Word::new([
            registry_id.suffix(),
            registry_id.prefix().into(),
            Felt::ZERO,
            Felt::ZERO,
        ]),
    );

    println!(
        "REGISTRY SUFFIX {} PREFIX {} TAG {} NOTE_ROOT_HASH {:?}",
        registry_id.suffix(),
        registry_id.prefix().as_felt(),
        NoteTag::with_account_target(*registry_id),
        get_register_note_root_hash()
    );

    let register_note_root = StorageSlot::with_value(
        slot_name("zoro::lp_local::register_note_root"),
        get_register_note_root_hash(),
    );
    let mut user_deposits_mapping = StorageMap::new();
    user_deposits_mapping.insert(
        StorageMapKey::new(Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE])),
        Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE]),
    )?;
    let user_deposits_slot = StorageSlot::with_map(
        slot_name("zoro::lp_local::user_deposits_mapping"),
        user_deposits_mapping,
    );

    let lp_local_component = zoro_component(
        lp_local_library,
        vec![
            assets_mapping_slot,
            reserve_slot,
            total_supply_slot,
            user_deposits_slot,
            registry_id_slot,
            register_note_root,
        ],
        "zoro::lp_local",
    )
    .map_err(|e| ClientError::AccountError(e))?;

    let xyk_pool_component = zoro_component(xyk_pool_library, vec![], "zoro::xyk_pool")
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(lp_local_component)
        .with_component(xyk_pool_component)
        .with_auth_component(NoAuth)
        .with_component(BasicWallet)
        .build() //_with_schema_commitment()
        .map_err(|e| anyhow!("Failed to build combined pool contract: {e:?}"))
        .unwrap();

    println!(
        "Combined pool deployed: lp_local + xyk_pool => ID: {:?}",
        contract.id().to_hex()
    );

    // keystore.add_key(&key_pair, contract.id()).await?;
    client.add_account(&contract.clone(), true).await?;
    client.sync_state().await?;

    touch_account(client, &contract).await.unwrap();

    // sleep(Duration::from_secs(5)).await;

    Ok((contract, key_pair))
}

/// Deploys an lp_local fuzz dummy account (generated from lp_local.masm with public mint/burn).
///
/// Storage slots (must match generated lp_local_fuzz_dummy):
///   - `reserve`: [0, 0, 0, 0]
///   - `total_supply`: [0, 0, 0, 0]
///   - `user_deposits_mapping`: empty map
pub async fn deploy_lp_local_fuzz_dummy(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let lp_local_fuzz_dummy_library = get_lp_local_fuzz_dummy_library()
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;
    let reserve_slot = StorageSlot::with_empty_value(slot_name("zoro::lp_local::reserve"));
    let total_supply_slot =
        StorageSlot::with_empty_value(slot_name("zoro::lp_local::total_supply"));
    let assets_mapping_slot =
        StorageSlot::with_empty_map(slot_name("zoro::lp_local::assets_mapping"));
    let mut user_deposits_mapping = StorageMap::new();
    user_deposits_mapping.insert(
        StorageMapKey::new(Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE])),
        Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ONE]),
    )?;
    let user_deposits_slot = StorageSlot::with_map(
        slot_name("zoro::lp_local::user_deposits_mapping"),
        user_deposits_mapping,
    );
    let registry_id_slot = StorageSlot::with_empty_value(slot_name("zoro::lp_local::registry_id"));
    let register_note_root =
        StorageSlot::with_empty_value(slot_name("zoro::lp_local::register_note_root"));

    let component = zoro_component(
        lp_local_fuzz_dummy_library,
        vec![
            assets_mapping_slot,
            reserve_slot,
            total_supply_slot,
            user_deposits_slot,
            registry_id_slot,
            register_note_root,
        ],
        "zoro::lp_local",
    )
    .map_err(|e| ClientError::AccountError(e))?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(component)
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(BasicWallet)
        .build_with_schema_commitment()?;

    keystore.add_key(&key_pair, contract.id()).await.unwrap();
    client.add_account(&contract.clone(), false).await?;
    client.sync_state().await?;
    Ok((contract, key_pair))
}

/// Deploys a dummy account for storage_utils fuzz tests.
///
/// Storage slots (must match constants in storage_fuzz_dummy.masm):
///   - `value_slot`: value slot (initially empty)
///   - `map_slot`: map slot (initially empty)
pub async fn deploy_storage_fuzz_dummy(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
    initial_value: u64,
    initial_map_value: u64,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let dummy_code_path: PathBuf = [manifest_dir, "asm", "accounts", "storage_fuzz_dummy.masm"]
        .iter()
        .collect();
    let dummy_code = fs::read_to_string(&dummy_code_path)
        .unwrap_or_else(|err| panic!("unable to read from {dummy_code_path:?}: {err}"));

    let storage_utils_library = get_storage_utils_library()
        .unwrap_or_else(|e| panic!("Failed to get storage_utils library: {e:?}"));
    // let math_library =
    //     get_math_library().unwrap_or_else(|e| panic!("Failed to get math library: {e:?}"));
    let static_libs = [storage_utils_library.clone()];
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(storage_utils_library)
        .unwrap_or_else(|e| panic!("Failed to add math library: {e:?}"));

    let dummy_library = create_library(
        assembler.clone(),
        "zoro::storage_fuzz_dummy",
        &dummy_code,
        &static_libs,
    )
    .unwrap_or_else(|e| panic!("Failed to create storage_fuzz_dummy library: {e:?}"));

    let value_slot = StorageSlot::with_value(
        slot_name("zoro::storage_fuzz_dummy::value_slot"),
        Word::new([
            Felt::new_unchecked(initial_value),
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
        ]),
    );

    let key = Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ZERO]);
    let val = Word::new([
        Felt::new_unchecked(initial_map_value),
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
    ]);
    let mut mapping_instance = StorageMap::new();
    mapping_instance.insert(StorageMapKey::new(key), val)?;
    let map_slot = StorageSlot::with_map(
        slot_name("zoro::storage_fuzz_dummy::map_slot"),
        mapping_instance,
    );
    let lp_total_supply_slot = StorageSlot::with_value(
        slot_name("zoro::storage_fuzz_dummy::lp_total_supply"),
        Word::new([Felt::ZERO, Felt::ZERO, Felt::ZERO, Felt::ZERO]),
    );
    let lp_user_deposits_mapping = StorageSlot::with_empty_map(slot_name(
        "zoro::storage_fuzz_dummy::lp_user_deposits_mapping",
    ));

    let dummy_component = zoro_component(
        dummy_library,
        vec![
            value_slot,
            map_slot,
            lp_total_supply_slot,
            lp_user_deposits_mapping,
        ],
        "zoro::storage_fuzz_dummy",
    )?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let dummy_contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(dummy_component)
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(BasicWallet)
        .build_with_schema_commitment()?;

    keystore
        .add_key(&key_pair, dummy_contract.id())
        .await
        .unwrap();
    client.add_account(&dummy_contract.clone(), false).await?;
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok((dummy_contract, key_pair))
}

/// Deploys a registry account pre-seeded with accepted pool code hashes.
///
/// Storage slots (must match constants in registry.masm):
///   - `accepted_code_hashes_mapping`: map with each pool_code_hash → [1, 0, 0, 0]
///   - `pools_mapping`: empty map
///   - `assets_to_pool_mapping`: empty map
pub async fn deploy_registry(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
    accepted_pool_code_hashes: &[Word],
) -> Result<(Account, AuthSecretKey), ClientError> {
    let registry_library = get_registry_library()
        .map_err(|e| ClientError::NoteError(NoteError::other(e.to_string())))?;

    let mut accepted_hashes_map = StorageMap::new();

    for code_hash in accepted_pool_code_hashes {
        println!("Accepted pool code hash: {code_hash:?}");
        accepted_hashes_map.insert(
            StorageMapKey::new(*code_hash),
            Word::new([Felt::ONE, Felt::ZERO, Felt::ZERO, Felt::ZERO]),
        )?;
    }
    let accepted_hashes_slot = StorageSlot::with_map(
        slot_name("zoro::registry::accepted_code_hashes_mapping"),
        accepted_hashes_map,
    );

    let pools_mapping_slot =
        StorageSlot::with_empty_map(slot_name("zoro::registry::pools_mapping"));

    let assets_to_pool_mapping_slot =
        StorageSlot::with_empty_map(slot_name("zoro::registry::assets_to_pool_mapping"));

    let registry_component = zoro_component(
        registry_library,
        vec![
            pools_mapping_slot,
            assets_to_pool_mapping_slot,
            accepted_hashes_slot,
        ],
        "zoro::registry",
    )
    .map_err(|e| ClientError::AccountError(e))?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());

    let registry = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(registry_component)
        // .with_auth_component(auth_single_sig_component(&key_pair))
        .with_auth_component(NoAuth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .map_err(|e| anyhow!("Failed to build registry contract: {e:?}"))
        .unwrap();

    println!(
        "Registry deployed => ID: {}, accepted code hashes: {}",
        registry.id().to_hex(),
        accepted_pool_code_hashes.len(),
    );

    keystore.add_key(&key_pair, registry.id()).await.unwrap();
    client.add_account(&registry, true).await?;
    client.sync_state().await?;

    // let _ = touch_account(client, &registry).await;

    println!("Dummy register note ...");

    let dummy_register = build_dummy_register_note(&registry.id(), client.rng().draw_word());
    let init_note_tx = TransactionRequestBuilder::new()
        .own_output_notes([dummy_register])
        .build()?;

    println!("Dummy register note BUILT ");

    client
        .submit_new_transaction(registry.id(), init_note_tx)
        .await?;

    println!("Dummy register note sent");

    tokio::time::sleep(Duration::from_secs(5)).await;

    Ok((registry, key_pair))
}

#[derive(Deserialize, Debug)]
pub struct FaucetConfig {
    pub symbol: String,
    pub max_supply: u64,
    pub decimals: u8,
}
#[derive(Deserialize, Debug)]
pub struct FaucetsConfig {
    pub faucets: Vec<FaucetConfig>,
}
#[derive(Debug)]
pub struct Faucet {
    pub faucet: Account,
    pub config: FaucetConfig,
}

/// Load faucets config from `faucets.toml` in the manifest directory.
pub fn load_faucets_config() -> Result<FaucetsConfig> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "faucets.toml"].iter().collect();
    let s = fs::read_to_string(&path).map_err(|e| anyhow!("Error reading {path:?}: {e}"))?;
    toml::from_str(&s).map_err(Into::into)
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CachedFaucet {
    pub account_id_hex: String,
    pub symbol: String,
    pub decimals: u8,
    pub max_supply: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CachedTestState {
    pub faucets: Vec<CachedFaucet>,
    pub user_account_id_hex: String,
}

pub fn save_test_state(path: &PathBuf, state: &CachedTestState) -> Result<()> {
    let toml_str =
        toml::to_string_pretty(state).map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
    fs::write(path, toml_str).map_err(|e| anyhow!("Failed to write {path:?}: {e}"))?;
    println!("Saved test state to {path:?}");
    Ok(())
}

/// Returns `None` if the file is missing or corrupt (logged as warning).
pub fn load_test_state(path: &PathBuf) -> Option<CachedTestState> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return None,
    };
    match toml::from_str(&content) {
        Ok(state) => {
            println!("Loaded cached test state from {path:?}");
            Some(state)
        }
        Err(e) => {
            warn!("Corrupt test state at {path:?}: {e}");
            None
        }
    }
}

/// Imports a public account from the network by its ID.
/// Returns the full `Account` fetched via RPC.
pub async fn try_import_account(clients: &mut MidenClients, id: AccountId) -> Result<Account> {
    clients.client.import_account_by_id(id.clone()).await?;

    let account = clients
        .client
        .get_account(id)
        .await?
        .ok_or(anyhow!("No account found on chain for account_id {}", id))?;

    Ok(account)
}

/// Deploys a single simple fungible faucet. Does not read any config files.
/// Returns the deployed faucet account.
pub async fn deploy_simple_faucet(
    client: &mut MidenClient,
    keystore: &FilesystemKeyStore,
    symbol: &str,
    decimals: u8,
    max_supply: u64,
) -> Result<Account> {
    let symbol =
        TokenSymbol::new(symbol).map_err(|e| anyhow!("Failed to create token symbol: {e:?}"))?;
    let name = TokenName::new(&symbol.to_string())
        .map_err(|e| anyhow!("Failed to create token name: {e:?}"))?;

    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(decimals)
        .max_supply(AssetAmount::new(max_supply)?)
        .build()
        .map_err(|e| anyhow!("Failed to build fungible faucet component: {e:?}"))?;
    let policy_manager = TokenPolicyManager::new()
        .with_mint_policy(MintPolicyConfig::AllowAll, PolicyRegistration::Active)
        .map_err(|e| anyhow!("Failed to configure mint policy: {e:?}"))?
        .with_burn_policy(BurnPolicyConfig::AllowAll, PolicyRegistration::Active)
        .map_err(|e| anyhow!("Failed to configure burn policy: {e:?}"))?;

    let faucet_account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(auth_single_sig_component(&key_pair))
        .with_component(faucet)
        .with_components(policy_manager)
        .build()
        .map_err(|e| anyhow!("Failed to build faucet account: {e:?}"))?;

    client.add_account(&faucet_account, true).await?;
    keystore
        .add_key(&key_pair, faucet_account.id())
        .await
        .map_err(|e| anyhow!("Failed to add key to keystore: {e:?}"))?;

    client.sync_state().await?;
    // dummy tx to get the new account into node
    touch_account(client, &faucet_account).await.unwrap();

    Ok(faucet_account)
}

/// Reads `faucets.toml` from the manifest directory and deploys each configured faucet.
/// Returns the deployed faucet accounts in config order.
pub async fn deploy_simple_faucets_from_config(
    client: &mut MidenClient,
    keystore: &FilesystemKeyStore,
) -> Result<Vec<Faucet>> {
    let sync_summary = client.sync_state().await?;
    println!("Latest block: {}", sync_summary.block_num);

    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let faucet_config_path: PathBuf = [manifest_dir, "faucets.toml"].iter().collect();
    let faucet_config = fs::read_to_string(&faucet_config_path)
        .map_err(|e| anyhow!("Error opening {faucet_config_path:?}: {e}"))?;

    let faucet_config: FaucetsConfig = toml::from_str(&faucet_config)?;

    let mut accounts = Vec::with_capacity(faucet_config.faucets.len());
    println!("faucet_config.faucets: {:?}", faucet_config.faucets);
    for faucet in faucet_config.faucets {
        println!("Deploying faucet {}.", faucet.symbol);
        let account = deploy_simple_faucet(
            client,
            keystore,
            &faucet.symbol,
            faucet.decimals,
            faucet.max_supply,
        )
        .await?;

        println!(
            "Faucet {} successfully deployed -> ID {:?}",
            faucet.symbol,
            // account.id().to_bech32(clients.endpoint.to_network_id()),
            account.id().to_hex(),
        );

        accounts.push(Faucet {
            faucet: account,
            config: faucet,
        });
    }

    println!("All faucets deployed successfully.");
    Ok(accounts)
}

pub async fn fund_wallet(
    clients: &mut MidenClients,
    account: &Account,
    asset: &FaucetConfig,
    asset_id: &AccountId,
    amount: u64,
) -> Result<()> {
    let client = &mut clients.client;
    let amount: u64 = if amount > 0 {
        amount
    } else {
        5 * 10u64.pow(asset.decimals as u32 - 2)
    }; // 0.05
    let fungible_asset = FungibleAsset::new(asset_id.clone(), amount)?;
    client.import_account_by_id(asset_id.clone()).await?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        account.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let tx_id = client
        .submit_new_transaction(asset_id.clone(), transaction_request)
        .await?;
    println!("Minted {amount} {} for the user.", asset.symbol);
    client.sync_state().await?;

    let transaction = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .with_context(|| "failed to find transaction {tx_id:?} after submission")
        .unwrap();
    let minted_note = notes_from_output(&transaction.details.output_notes)
        .next()
        .cloned()
        .with_context(|| format!("failed to find full output note for transaction {tx_id:?}"))?;

    wait_for_note(client, &minted_note).await?;

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(minted_note.clone(), None)])
        .build()?;

    let _tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(&clients.rpc_api, asset_id).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed p2id note into its wallet");

    Ok(())
}

/// Waits for a specific note to become consumable.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `_account_id`: Account ID (unused but kept for API compatibility)
/// * `expected`: The note to wait for
pub async fn wait_for_note(client: &mut MidenClient, expected: &Note) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let notes = client.get_consumable_notes(None).await?;
        let found = notes.iter().any(|(rec, _)| rec.id() == Some(expected.id()));
        if found {
            info!("Note found {}", expected.id().to_hex());
            break;
        }
        debug!("Note {} not found. Waiting...", expected.id().to_hex());
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

pub fn get_return_note_serial(input_note_serial: Word, _user_id: AccountId) -> Word {
    let mut serial = Word::new([
        input_note_serial[3] + Felt::ONE,
        input_note_serial[2],
        input_note_serial[1],
        input_note_serial[0],
    ]);
    serial.reverse();
    serial
}

pub async fn touch_account(client: &mut MidenClient, account: &Account) -> Result<()> {
    let transaction_request = TransactionRequestBuilder::new().build()?;
    let _tx_id = client
        .submit_new_transaction(account.id(), transaction_request)
        .await?;
    client.sync_state().await?;
    Ok(())
}
