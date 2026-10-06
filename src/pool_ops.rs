use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use crate::utils::{get_p2id_root_hash, read_masm_to_string};
use anyhow::{anyhow, Result};
use miden_client::{
    account::AccountId,
    assembly::{
        Assembler, DefaultSourceManager, Library, MastForest, Module, ModuleKind,
        Path as AssemblyPath,
    },
    asset::FungibleAsset,
    note::{Note, NoteAssets, NoteRecipient, NoteTag, NoteType, PartialNoteMetadata},
    Felt, Word,
};
use miden_protocol::{
    assembly::LibraryExport,
    note::{NoteScript, NoteStorage},
    transaction::{TransactionKernel, TransactionScript},
    vm::Program,
};
use miden_standards::StandardsLib;

pub fn shared_source_manager() -> Arc<DefaultSourceManager> {
    static SOURCE_MANAGER: LazyLock<Arc<DefaultSourceManager>> =
        LazyLock::new(|| Arc::new(DefaultSourceManager::default()));
    SOURCE_MANAGER.clone()
}

pub fn kernel_assembler() -> Assembler {
    TransactionKernel::assembler_with_source_manager(shared_source_manager())
}

/// `Assembler::with_static_library` inlines a statically-linked library's MAST nodes into the
/// new build, but `MastForestBuilder::new` only copies over that library's advice map — not its
/// registered error codes. Without this, any `assert.err=CONST` defined in a statically-linked
/// helper library (e.g. `lp_local`'s `ERR_UNKNOWN_ASSET`) shows up at runtime as a bare numeric
/// error code instead of the original message, since the executing MastForest's error-code map
/// never received the string. This re-attaches those mappings after assembly. It only touches
/// debug info (not the MAST nodes/exports), so the library's digest is unaffected.
fn merge_static_error_codes(library: Arc<Library>, static_libs: &[Arc<Library>]) -> Arc<Library> {
    if static_libs.is_empty() {
        return library;
    }
    let mut forest: MastForest = library.mast_forest().as_ref().clone();
    for lib in static_libs {
        let codes: Vec<(u64, Arc<str>)> = lib
            .mast_forest()
            .debug_info()
            .error_codes()
            .map(|(code, msg)| (*code, msg.clone()))
            .collect();
        forest.debug_info_mut().extend_error_codes(codes);
    }
    let exports: BTreeMap<Arc<AssemblyPath>, LibraryExport> = library
        .exports()
        .map(|export| (export.path(), export.clone()))
        .collect();
    Arc::new(
        Library::new(Arc::new(forest), exports)
            .expect("merging error codes must not change exports or MAST roots"),
    )
}

/// Same fix as [`merge_static_error_codes`], but for an assembled [`Program`] (e.g. transaction
/// scripts), which go through the same static-linking code path.
fn merge_static_error_codes_into_program(
    program: Program,
    static_libs: &[Arc<Library>],
) -> Program {
    if static_libs.is_empty() {
        return program;
    }
    let mut forest: MastForest = program.mast_forest().as_ref().clone();
    for lib in static_libs {
        let codes: Vec<(u64, Arc<str>)> = lib
            .mast_forest()
            .debug_info()
            .error_codes()
            .map(|(code, msg)| (*code, msg.clone()))
            .collect();
        forest.debug_info_mut().extend_error_codes(codes);
    }
    Program::with_kernel(
        Arc::new(forest),
        program.entrypoint(),
        program.kernel().clone(),
    )
}

pub fn create_library(
    assembler: Assembler,
    library_path: &str,
    source_code: &str,
    static_libs: &[Arc<Library>],
) -> Result<Arc<Library>, Box<dyn std::error::Error>> {
    let source_manager = shared_source_manager();
    let module = Module::parser(ModuleKind::Library).parse_str(
        AssemblyPath::new(library_path),
        source_code,
        source_manager,
    )?;
    let library = assembler.assemble_library([module])?;
    Ok(merge_static_error_codes(library, static_libs))
}

/// Compiles the asset_utils MASM library (expand_asset, compress_asset).
pub fn get_asset_utils_library() -> Result<Arc<Library>> {
    let source = read_masm_to_string("accounts", "asset_utils")?;
    let assembler = kernel_assembler().with_warnings_as_errors(true);
    create_library(assembler, "zoro::asset_utils", &source, &[])
        .map_err(|e| anyhow!("Failed to compile asset_utils library: {e:?}"))
}

fn compile_note_script(
    library_path: &str,
    source: &str,
    static_libs: &[Arc<Library>],
) -> Result<NoteScript> {
    let mut assembler = kernel_assembler().with_warnings_as_errors(true);
    for lib in static_libs {
        assembler = assembler
            .with_static_library(lib.clone())
            .map_err(|e| anyhow!("Failed to add static library: {e:?}"))?;
    }
    let library = create_library(assembler, library_path, source, static_libs)
        .map_err(|e| anyhow!("Failed to compile note script library: {e:?}"))?;
    NoteScript::from_library(&library).map_err(|e| anyhow!("Failed to create note script: {e:?}"))
}

/// Compiles the pool MASM library from source.
pub fn get_pool_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let lp_local_library = get_lp_local_library()?;
    let asset_utils_library = get_asset_utils_library()?;
    let static_libs = [
        math_library.clone(),
        asset_utils_library.clone(),
        lp_local_library.clone(),
    ];
    let source = read_masm_to_string("accounts", "xyk_pool")?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(math_library)
        .map_err(|e| anyhow!("Failed to add math library to assembler: {e:?}"))?
        // .with_static_library(storage_utils_library)
        // .map_err(|e| anyhow!("Failed to add storage_utils library to assembler: {e:?}"))?
        .with_static_library(asset_utils_library)
        .map_err(|e| anyhow!("Failed to add asset_utils library to assembler: {e:?}"))?
        .with_static_library(lp_local_library)
        .map_err(|e| anyhow!("Failed to add lp_local library to assembler: {e:?}"))?;
    create_library(assembler, "zoro::xyk_pool", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile pool library: {e:?}"))
}

/// Compiles the math MASM library (sqrt, mul_div, safe_sub, etc.).
pub fn get_math_library() -> Result<Arc<Library>> {
    let source = read_masm_to_string("accounts", "math")?;
    let assembler = kernel_assembler().with_warnings_as_errors(true);
    create_library(assembler, "zoro::math", &source, &[])
        .map_err(|e| anyhow!("Failed to compile math library: {e:?}"))
}

/// Compiles the lp_local MASM library (get_lp_amount_out, deposit, withdraw, etc.).
/// Depends on math and storage_utils libraries.
pub fn get_lp_local_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let storage_utils_library = get_storage_utils_library()?;
    let asset_utils_library = get_asset_utils_library()?;

    let static_libs = [
        math_library.clone(),
        storage_utils_library.clone(),
        asset_utils_library.clone(),
    ];
    let source = read_masm_to_string("accounts", "lp_local")?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_dynamic_library(StandardsLib::default())
        .map_err(|e| anyhow!("Failed to add standards library to assembler: {e:?}"))?
        .with_static_library(math_library)
        .map_err(|e| anyhow!("Failed to add math library to assembler: {e:?}"))?
        .with_static_library(storage_utils_library)
        .map_err(|e| anyhow!("Failed to add storage_utils library to assembler: {e:?}"))?
        .with_static_library(asset_utils_library)
        .map_err(|e| anyhow!("Failed to add asset_utils library to assembler: {e:?}"))?;
    create_library(assembler, "zoro::lp_local", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile lp_local library: {e:?}"))
}

/// Generates the lp_local fuzz dummy library by reading lp_local.masm and transforming it:
/// - Makes mint and burn public for fuzz testing
/// - Adds get_user_deposit helper for verification
fn generate_lp_local_fuzz_dummy_source() -> Result<String> {
    let source = read_masm_to_string("accounts", "lp_local")?;

    let source = source.replace("proc mint#", "pub proc mint#");
    let source = source.replace("proc burn#", "pub proc burn#");

    Ok(source)
}

/// Compiles the lp_local fuzz dummy library (generated from lp_local.masm with public mint/burn).
pub fn get_lp_local_fuzz_dummy_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let storage_utils_library = get_storage_utils_library()?;
    let asset_utils_library = get_asset_utils_library()?;
    let static_libs = [
        math_library.clone(),
        storage_utils_library.clone(),
        asset_utils_library.clone(),
    ];
    let source = generate_lp_local_fuzz_dummy_source()?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_dynamic_library(StandardsLib::default())
        .map_err(|e| anyhow!("Failed to add standards library to assembler: {e:?}"))?
        .with_static_library(math_library)
        .map_err(|e| anyhow!("Failed to add math library to assembler: {e:?}"))?
        .with_static_library(storage_utils_library)
        .map_err(|e| anyhow!("Failed to add storage_utils library to assembler: {e:?}"))?
        .with_static_library(asset_utils_library)
        .map_err(|e| anyhow!("Failed to add asset_utils library to assembler: {e:?}"))?;
    create_library(assembler, "zoro::lp_local", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile lp_local_fuzz_dummy library: {e:?}"))
}

/// Compiles a transaction script for lp_local mint/burn fuzz tests.
pub fn compile_lp_local_fuzz_tx_script(source: &str) -> Result<TransactionScript> {
    let lp_local_fuzz_dummy_library = get_lp_local_fuzz_dummy_library()?;
    compile_custom_tx_script(&lp_local_fuzz_dummy_library, source)
}

/// Compiles the registry MASM library (order_assets, register_pool, etc.).
/// Depends on math and storage_utils libraries.
pub fn get_registry_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let storage_utils_library = get_storage_utils_library()?;
    let xyk_pool_library = get_combined_pool_library()?;
    let static_libs = [
        math_library.clone(),
        storage_utils_library.clone(),
        xyk_pool_library.clone(),
    ];
    let source = read_masm_to_string("accounts", "registry")?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(math_library)
        .map_err(|e| anyhow!("Failed to add math library to assembler: {e:?}"))?
        .with_static_library(storage_utils_library)
        .map_err(|e| anyhow!("Failed to add storage_utils library to assembler: {e:?}"))?
        .with_static_library(xyk_pool_library)
        .map_err(|e| anyhow!("Failed to add xyk_pool library to assembler: {e:?}"))?;
    create_library(assembler, "zoro::registry", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile registry library: {e:?}"))
}

/// Compiles the storage_utils MASM library (add_to_storage_item, add_to_map_item, set_map_item).
/// Depends on the math library.
pub fn get_storage_utils_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let static_libs = [math_library.clone()];
    let source = read_masm_to_string("accounts", "storage_utils")?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(math_library)
        .unwrap_or_else(|e| panic!("Failed to add math library to assembler: {e:?}"));
    create_library(assembler, "zoro::storage_utils", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile storage_utils library: {e:?}"))
}

/// Compiles the storage_fuzz_dummy MASM library (minimal dummy with slot constants).
pub fn get_storage_fuzz_dummy_library() -> Result<Arc<Library>> {
    let storage_utils_library = get_storage_utils_library()?;
    let static_libs = [storage_utils_library.clone()];
    let source = read_masm_to_string("accounts", "storage_fuzz_dummy")?;

    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(storage_utils_library)
        .unwrap_or_else(|e| panic!("Failed to add storage_utils library to assembler: {e:?}"));
    create_library(assembler, "zoro::storage_fuzz_dummy", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile storage_fuzz_dummy library: {e:?}"))
}

/// Compiles a transaction script for storage fuzz tests.
/// Links both storage_utils and storage_fuzz_dummy libraries.
pub fn compile_storage_fuzz_tx_script(source: &str) -> Result<TransactionScript> {
    let storage_utils_library = get_storage_utils_library()?;
    let storage_fuzz_dummy_library = get_storage_fuzz_dummy_library()?;
    let static_libs = [
        storage_utils_library.clone(),
        storage_fuzz_dummy_library.clone(),
    ];
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(storage_utils_library)
        .map_err(|e| anyhow!("Failed to add storage_utils library: {e:?}"))?
        .with_static_library(storage_fuzz_dummy_library)
        .map_err(|e| anyhow!("Failed to add storage_fuzz_dummy library: {e:?}"))?;
    let program = assembler
        .assemble_program(source)
        .map_err(|e| anyhow!("Failed to compile storage fuzz script: {e:?}"))?;
    let program = merge_static_error_codes_into_program(program, &static_libs);
    Ok(TransactionScript::new(program))
}

/// Compiles a transaction script from arbitrary MASM source, linked against the pool library.
pub fn compile_custom_tx_script(
    pool_library: &Arc<Library>,
    source: &str,
) -> Result<TransactionScript> {
    let static_libs = [pool_library.clone()];
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(pool_library.clone())
        .map_err(|e| anyhow!("Failed to add pool library to assembler: {e:?}"))?;
    let program = assembler
        .assemble_program(source)
        .map_err(|e| anyhow!("Failed to compile tx script: {e:?}"))?;
    let program = merge_static_error_codes_into_program(program, &static_libs);
    Ok(TransactionScript::new(program))
}

/// Compiles a transaction script that calls the given pool procedure via `call`.
pub fn compile_pool_tx_script(
    pool_library: &Arc<Library>,
    procedure_name: &str,
) -> Result<TransactionScript> {
    let source = format!("use zoro::xyk_pool\nbegin\n    exec.xyk_pool::{procedure_name}\nend");
    compile_custom_tx_script(pool_library, &source)
}

/// Compiles the lp_local deposit note script.
/// The script loads assets and user_id from the note via active_note::get_assets/get_inputs,
/// then calls lp_local::deposit with [ASSET0, ASSET1, user_id_prefix, user_id_suffix].
pub fn compile_lp_local_deposit_note_script(lp_local_library: &Arc<Library>) -> Result<NoteScript> {
    let asset_utils_library = get_asset_utils_library()?;
    let source = read_masm_to_string("notes", "xyk_deposit")
        .map_err(|e| anyhow!("Failed to read xyk_deposit note script: {e:?}"))?;
    compile_note_script(
        "note::xyk_deposit",
        &source,
        &[lp_local_library.clone(), asset_utils_library],
    )
}

/// Compiles the register xyk pool note script.
pub fn compile_xyk_register_note_script() -> Result<NoteScript> {
    let xyk_pool_lib = get_combined_pool_library()?;
    let xyk_registry_lib = get_registry_library()?;
    let source = read_masm_to_string("notes", "xyk_register")
        .map_err(|e| anyhow!("Failed to read xyk_register note script: {e:?}"))?;
    compile_note_script(
        "note::xyk_register",
        &source,
        &[xyk_pool_lib, xyk_registry_lib],
    )
}

/// Builds a deposit note targeting the lp_local pool.
/// Note inputs: [user_id_prefix, user_id_suffix].
pub fn build_lp_local_deposit_note(
    pool_id: AccountId,
    lp_local_library: &Arc<Library>,
    token0_asset: FungibleAsset,
    token1_asset: FungibleAsset,
    user_id: AccountId,
    sender: AccountId,
    serial_num: Word,
) -> Result<Note> {
    let script = compile_lp_local_deposit_note_script(lp_local_library)?;
    let storage = NoteStorage::new(vec![user_id.prefix().into(), user_id.suffix()])?;
    let assets = NoteAssets::new(vec![token0_asset.into(), token1_asset.into()])?;
    let tag = NoteTag::with_account_target(pool_id);
    let metadata = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag);
    let recipient = NoteRecipient::new(serial_num, script, storage);
    Ok(Note::new(assets, metadata, recipient))
}

/// Compiles the lp_local withdraw note script.
/// The script reads note inputs and calls lp_local::withdraw with
/// [LP_AMOUNT_WORD, user_id_prefix, user_id_suffix, note_tag, note_type, RECIPIENT_WORD].
pub fn compile_lp_local_withdraw_note_script(
    lp_local_library: &Arc<Library>,
) -> Result<NoteScript> {
    let source = read_masm_to_string("notes", "xyk_withdraw")
        .map_err(|e| anyhow!("Failed to read xyk_withdraw note script: {e:?}"))?;
    compile_note_script(
        "note::xyk_withdraw",
        &source,
        std::slice::from_ref(lp_local_library),
    )
}

/// Builds a withdraw note targeting the lp_local pool.
/// Note inputs: [lp_amount, 0, 0, 0,  note_tag, note_type, 0, 0,  r0, r1, r2, r3].
pub fn build_lp_local_withdraw_note(
    pool_id: AccountId,
    lp_local_library: &Arc<Library>,
    lp_amount: u64,
    sender: AccountId,
    return_note_tag: Felt,
    return_note_type: Felt,
    withdraw_note_serial: Word,
) -> Result<Note> {
    let return_note_root_hash = get_p2id_root_hash();
    let script = compile_lp_local_withdraw_note_script(lp_local_library)?;
    let storage = NoteStorage::new(vec![
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::new(lp_amount)?,
        return_note_tag,
        return_note_type,
        Felt::ZERO,
        Felt::ZERO,
        return_note_root_hash[0],
        return_note_root_hash[1],
        return_note_root_hash[2],
        return_note_root_hash[3],
    ])?;
    let assets = NoteAssets::new(vec![])?;
    let tag = NoteTag::with_account_target(pool_id);
    let metadata = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag);
    let recipient = NoteRecipient::new(withdraw_note_serial, script, storage);
    Ok(Note::new(assets, metadata, recipient))
}

/// Compiles a note script that calls the given pool procedure via `call`.
pub fn compile_pool_note_script(
    pool_library: &Arc<Library>,
    procedure_name: &str,
) -> Result<NoteScript> {
    let source = format!(
        "@note_script\nuse zoro::xyk_pool\npub proc main\n    call.xyk_pool::{procedure_name}\nend"
    );
    compile_note_script(
        &format!("note::xyk_pool::{procedure_name}"),
        &source,
        &[pool_library.clone()],
    )
}

/// Compiles the xyk_pool library with lp_local, math, and storage_utils as dependencies.
/// Used when deploying a combined pool (lp_local + xyk_pool on the same account).
pub fn get_combined_pool_library() -> Result<Arc<Library>> {
    let math_library = get_math_library()?;
    let storage_utils_library = get_storage_utils_library()?;
    let asset_utils_library = get_asset_utils_library()?;
    let lp_local_library = get_lp_local_library()?;
    let static_libs = [
        math_library.clone(),
        storage_utils_library.clone(),
        asset_utils_library.clone(),
        lp_local_library.clone(),
    ];

    let source = read_masm_to_string("accounts", "xyk_pool")?;
    let assembler = kernel_assembler()
        .with_warnings_as_errors(true)
        .with_static_library(math_library)
        .map_err(|e| anyhow!("Failed to add math library: {e:?}"))?
        .with_static_library(storage_utils_library)
        .map_err(|e| anyhow!("Failed to add storage_utils library: {e:?}"))?
        .with_static_library(asset_utils_library)
        .map_err(|e| anyhow!("Failed to add asset_utils library: {e:?}"))?
        .with_static_library(lp_local_library)
        .map_err(|e| anyhow!("Failed to add lp_local library: {e:?}"))?;
    create_library(assembler, "zoro::xyk_pool", &source, &static_libs)
        .map_err(|e| anyhow!("Failed to compile combined pool library: {e:?}"))
}

/// Compiles the xyk_swap_exact_tokens_for_tokens note script, linked against the combined pool library.
pub fn compile_xyk_swap_exact_tokens_for_tokens_note_script(
    xyk_pool_library: &Arc<Library>,
) -> Result<NoteScript> {
    let asset_utils_library = get_asset_utils_library()?;
    let source = read_masm_to_string("notes", "xyk_swap_exact_tokens_for_tokens").map_err(|e| {
        anyhow!("Failed to read xyk_swap_exact_tokens_for_tokens note script: {e:?}")
    })?;
    compile_note_script(
        "note::xyk_swap_exact_tokens_for_tokens",
        &source,
        &[xyk_pool_library.clone(), asset_utils_library],
    )
}

pub fn build_dummy_register_note(registry_id: &AccountId, serial_num: Word) -> Note {
    let script = compile_xyk_register_note_script().unwrap();
    let assets = NoteAssets::new(vec![]).unwrap();
    let tag = NoteTag::new(0);
    let metadata = PartialNoteMetadata::new(*registry_id, NoteType::Public).with_tag(tag);
    let storage = NoteStorage::new(
        [
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
        ]
        .into(),
    )
    .unwrap();
    let recipient = NoteRecipient::new(serial_num, script, storage);
    Note::new(assets, metadata, recipient)
}

/// Builds a xyk_register note targeting the registry
pub fn build_xyk_register_note(
    registry_id: &AccountId,
    serial_num: Word,
    token0: &AccountId,
    token1: &AccountId,
    xyk_pool: &AccountId,
    sender: &AccountId,
) -> Result<Note> {
    let script = compile_xyk_register_note_script()?;

    let inputs = NoteStorage::new(vec![
        token0.prefix().into(),
        token0.suffix(),
        token1.prefix().into(),
        token1.suffix(),
        xyk_pool.prefix().into(),
        xyk_pool.suffix(),
        registry_id.prefix().into(),
        registry_id.suffix(),
    ])?;

    let assets = NoteAssets::new(vec![])?;
    let tag = NoteTag::new(0);
    let metadata = PartialNoteMetadata::new(*sender, NoteType::Public).with_tag(tag);
    let recipient = NoteRecipient::new(serial_num, script.clone(), inputs.clone());
    println!(
        "REGISTER NOTE recipient: {:?}, serial: {:?}, script {:?}, storage {:?}, tag: {:?}, registry_prefix: {:?}",
        recipient.digest(),
        serial_num,
        script.root(),
        inputs,
        tag,
        registry_id.prefix().as_felt().as_canonical_u64()
    );

    Ok(Note::new(assets, metadata, recipient))
}

/// Builds a swap note targeting the combined pool (lp_local + xyk_pool).
///
/// Note inputs layout (12 felts):
///   word 0: [0, 0, 0, min_amount_out]          - MIN_ASSET_OUT
///   word 1: [deadline, note_tag, note_type, 0]  - scalars
///   word 2: [r0, r1, r2, r3]                   - RECIPIENT digest
pub fn build_xyk_swap_exact_tokens_for_tokens_note(
    pool_id: AccountId,
    xyk_pool_library: &Arc<Library>,
    input_asset: FungibleAsset,
    min_output_asset: FungibleAsset,
    deadline: u64,
    sender: AccountId,
    return_note_tag: Felt,
    return_note_type: Felt,
    serial_num: Word,
) -> Result<Note> {
    let script = compile_xyk_swap_exact_tokens_for_tokens_note_script(xyk_pool_library)?;
    let p2id_root = get_p2id_root_hash();
    let storage = NoteStorage::new(vec![
        min_output_asset.faucet_id().suffix(),
        min_output_asset.faucet_id().prefix().as_felt(),
        Felt::ZERO,
        Felt::new(min_output_asset.amount().as_u64())?,
        Felt::new(deadline)?,
        return_note_tag,
        return_note_type,
        Felt::ZERO,
        p2id_root[0],
        p2id_root[1],
        p2id_root[2],
        p2id_root[3],
    ])?;

    let assets = NoteAssets::new(vec![input_asset.into()])?;
    let tag = NoteTag::with_account_target(pool_id);
    let metadata = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag);
    let recipient = NoteRecipient::new(serial_num, script, storage);
    Ok(Note::new(assets, metadata, recipient))
}

/// Compiles the xyk_swap_tokens_for_exact_tokens note script, linked against the combined pool library.
pub fn compile_xyk_swap_tokens_for_exact_tokens_note_script(
    xyk_pool_library: &Arc<Library>,
) -> Result<NoteScript> {
    let asset_utils_library = get_asset_utils_library()?;
    let source = read_masm_to_string("notes", "xyk_swap_tokens_for_exact_tokens").map_err(|e| {
        anyhow!("Failed to read xyk_swap_tokens_for_exact_tokens note script: {e:?}")
    })?;
    compile_note_script(
        "note::xyk_swap_tokens_for_exact_tokens",
        &source,
        &[xyk_pool_library.clone(), asset_utils_library],
    )
}

/// Builds a swap note targeting the combined pool (lp_local + xyk_pool).
///
/// Note inputs layout (12 felts):
///   word 0: [aset_out_prefix, aset_out_suffix, 0, amount_out]  - ASSET_OUT (exact output)
///   word 1: [deadline, note_tag, note_type, 0]                  - scalars
///   word 2: [r0, r1, r2, r3]                                   - RECIPIENT digest
pub fn build_xyk_swap_tokens_for_exact_tokens_note(
    pool_id: AccountId,
    xyk_pool_library: &Arc<Library>,
    max_input_asset: FungibleAsset,
    exact_output_asset: FungibleAsset,
    deadline: u64,
    sender: AccountId,
    return_note_tag: Felt,
    return_note_type: Felt,
    serial_num: Word,
) -> Result<Note> {
    let script = compile_xyk_swap_tokens_for_exact_tokens_note_script(xyk_pool_library)?;
    let p2id_root = get_p2id_root_hash();
    let storage = NoteStorage::new(vec![
        exact_output_asset.faucet_id().suffix(),
        exact_output_asset.faucet_id().prefix().as_felt(),
        Felt::ZERO,
        Felt::new(exact_output_asset.amount().as_u64())?,
        Felt::new(deadline)?,
        return_note_tag,
        return_note_type,
        Felt::ZERO,
        p2id_root[0],
        p2id_root[1],
        p2id_root[2],
        p2id_root[3],
    ])?;

    let assets = NoteAssets::new(vec![max_input_asset.into()])?;
    let tag = NoteTag::with_account_target(pool_id);
    let metadata = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag);
    let recipient = NoteRecipient::new(serial_num, script, storage);
    Ok(Note::new(assets, metadata, recipient))
}

// ---------------------------------------------------------------------------
// Math helpers
// ---------------------------------------------------------------------------

/// Expected LP tokens for a deposit.
pub fn compute_expected_lp(
    amount0: u64,
    amount1: u64,
    reserve0: u64,
    reserve1: u64,
    total_lp: u64,
) -> u64 {
    if total_lp == 0 {
        let product = amount0 as u128 * amount1 as u128;
        isqrt(product) as u64 - 100
    } else {
        let lp0 = (amount0 as u128 * total_lp as u128 / reserve0 as u128) as u64;
        let lp1 = (amount1 as u128 * total_lp as u128 / reserve1 as u128) as u64;
        lp0.min(lp1)
    }
}

/// Expected output amounts for a withdraw.
pub fn compute_expected_withdraw(
    total_supply: u64,
    lp_amount: u64,
    reserve_0: u64,
    reserve_1: u64,
) -> (u64, u64) {
    let amount_0 = lp_amount as u128 * reserve_0 as u128 / total_supply as u128;
    let amount_1 = lp_amount as u128 * reserve_1 as u128 / total_supply as u128;
    (amount_0 as u64, amount_1 as u64)
}

fn u128_mul(a: u128, b: u128) -> u128 {
    a.checked_mul(b).expect("product does not fit in u128")
}

fn felt_from_quotient(q: u128) -> u64 {
    let felt_max = u64::MAX - u32::MAX as u64;
    assert!(q <= felt_max as u128, "quotient does not fit in a felt");
    q as u64
}

/// Expected output amount for a swap (0.3% fee).
/// Panics when `amount_in * 997 * reserve_out` does not fit in a u128.
pub fn get_amount_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> u64 {
    let fee_adjusted = u128_mul(amount_in as u128, 997);
    let numerator = u128_mul(fee_adjusted, reserve_out as u128);
    let denominator = u128_mul(reserve_in as u128, 1000)
        .checked_add(fee_adjusted)
        .expect("denominator overflow");
    assert!(denominator != 0, "division by zero");
    felt_from_quotient(numerator / denominator)
}

/// Expected input amount for a swap (0.3% fee).
/// Panics when `amount_out * 1000 * reserve_in` does not fit in a u128.
pub fn get_amount_in(amount_out: u64, reserve_in: u64, reserve_out: u64) -> u64 {
    let amount_out_scaled = u128_mul(amount_out as u128, 1000);
    let numerator = u128_mul(amount_out_scaled, reserve_in as u128);
    let remaining = (reserve_out as u128)
        .checked_sub(amount_out as u128)
        .expect("underflow");
    let denominator = u128_mul(remaining, 997);
    assert!(denominator != 0, "division by zero");
    felt_from_quotient(numerator / denominator)
}

/// Integer square root (Newton's method, floor).
pub fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isqrt() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(10), 3);
        assert_eq!(isqrt(10_000 * 50_000), 22360);
    }

    #[test]
    fn test_swap_output() {
        let out = get_amount_out(1_000, 50_000, 50_000);
        assert!(out > 970 && out < 1000, "out={out}");
    }

    /// Largest `x` such that `x * factor * x` fits in a u128 and `x` is a felt.
    fn largest_symmetric_product(factor: u128) -> u64 {
        let mut lo = 1u64;
        let mut hi = u64::MAX - u32::MAX as u64;
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2;
            let fits = (mid as u128)
                .checked_mul(factor)
                .and_then(|v| v.checked_mul(mid as u128))
                .is_some();
            if fits {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    #[test]
    fn test_u128_mul_div_roundtrip() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state
        };
        let mut checked = 0;
        for _ in 0..200 {
            let d = (next() as u128) | 1;
            let q = next() as u128;
            let Some(prod) = q.checked_mul(d) else {
                continue;
            };
            let r = (next() as u128) % d;
            let Some(n) = prod.checked_add(r) else {
                continue;
            };
            assert_eq!(n / d, q);
            checked += 1;
        }
        assert!(checked > 0, "no fitting products were checked");
    }

    #[test]
    fn test_get_amount_out_near_u128_limit() {
        let x = largest_symmetric_product(997);
        let expected = (x as u128) * 997 * (x as u128) / ((x as u128) * 1000 + (x as u128) * 997);
        assert_eq!(get_amount_out(x, x, x), expected as u64);
        let overflow = x.checked_add(1).expect("room above the limit");
        assert!((overflow as u128)
            .checked_mul(997)
            .and_then(|v| v.checked_mul(overflow as u128))
            .is_none());
    }

    #[test]
    #[should_panic(expected = "product does not fit in u128")]
    fn test_get_amount_out_felt_max_overflows() {
        let felt_max = u64::MAX - u32::MAX as u64;
        let _ = get_amount_out(felt_max, felt_max, felt_max);
    }

    #[test]
    fn test_math_and_pool_libraries_compile() {
        get_math_library().expect("math library");
        get_lp_local_library().expect("lp_local library");
        get_pool_library().expect("pool library");
    }

    #[test]
    fn test_lp_local_deposit_note_script_compiles() {
        let lp_lib = get_lp_local_library().expect("lp_local library");
        let result = compile_lp_local_deposit_note_script(&lp_lib);
        assert!(
            result.is_ok(),
            "lp_local deposit note script: {:?}",
            result.err()
        );
    }

    fn run_vm(source: &str, libs: &[Arc<Library>]) -> Result<Vec<u64>, String> {
        let mut assembler = kernel_assembler().with_warnings_as_errors(true);
        for lib in libs {
            assembler = assembler
                .with_static_library(lib.clone())
                .map_err(|e| format!("link: {e:?}"))?;
        }
        let program = assembler
            .assemble_program(source)
            .map_err(|e| format!("assemble: {e:?}"))?;
        let program = merge_static_error_codes_into_program(program, libs);
        let mut host = miden_processor::DefaultHost::default();
        let core = miden_protocol::CoreLibrary::default();
        host.load_library(&core)
            .map_err(|e| format!("host: {e:?}"))?;
        let output = miden_processor::execute_sync(
            &program,
            miden_processor::StackInputs::new(&[]).expect("empty stack inputs"),
            miden_processor::advice::AdviceInputs::default(),
            &mut host,
            miden_processor::ExecutionOptions::default(),
        )
        .map_err(|e| e.to_string())?;
        Ok((0..8)
            .map(|i| {
                output
                    .stack
                    .get_element(i)
                    .expect("stack output")
                    .as_canonical_u64()
            })
            .collect())
    }

    fn felt_max() -> u64 {
        u64::MAX - u32::MAX as u64
    }

    #[test]
    fn test_vm_u128_mul_div_and_sqrt() {
        let math = get_math_library().expect("math library");
        let libs = [math];
        let f = felt_max();

        let small = run_vm(
            "use zoro::math\n\
             begin\n\
                 push.20.1.3\n\
                 exec.math::mul_div\n\
             end",
            &libs,
        )
        .expect("20/3");
        assert_eq!(small[0], 6);

        let hi = f >> 32;
        let lo = f as u32;
        let direct_cast = run_vm(
            &format!(
                "use zoro::math\n\
                 use miden::core::sys\n\
                 begin\n\
                     push.0.0.{hi}.{lo}\n\
                     exec.math::safe_cast_u128_into_felt\n\
                     exec.sys::truncate_stack\n\
                 end"
            ),
            &libs,
        )
        .expect("cast felt_max");
        assert_eq!(direct_cast[0], f);

        let high_limb = run_vm(
            "use zoro::math\n\
             begin\n\
                 push.0.1.0.0\n\
                 exec.math::safe_cast_u128_into_felt\n\
             end",
            &libs,
        );
        assert!(high_limb.is_err(), "a set high limb must not cast");

        let div_zero = run_vm(
            "use zoro::math\n\
             begin\n\
                 push.1.1.0\n\
                 exec.math::mul_div\n\
             end",
            &libs,
        );
        assert!(div_zero.is_err(), "division by zero must fail");

        for n in [0u64, 1, 2, 10, 144, 10_000 * 50_000, f] {
            let got = run_vm(
                &format!(
                    "use zoro::math\n\
                     begin\n\
                         push.{n}\n\
                         exec.math::sqrt\n\
                     end"
                ),
                &libs,
            )
            .unwrap_or_else(|e| panic!("sqrt({n}): {e}"));
            assert_eq!(got[0], isqrt(n as u128) as u64, "sqrt({n})");
        }

        let cast = run_vm(
            &format!(
                "use zoro::math\n\
                 begin\n\
                     push.{f}.1.1\n\
                     exec.math::mul_div\n\
                 end"
            ),
            &libs,
        )
        .expect("felt_max/1");
        assert_eq!(cast[0], f, "felt_max/1 stack={cast:?}");

        let mul = run_vm(
            &format!(
                "use zoro::math\n\
                 begin\n\
                     push.{f}.{f}.{f}\n\
                     exec.math::mul_div\n\
                 end"
            ),
            &libs,
        )
        .expect("mul_div felt_max");
        assert_eq!(mul[0], f, "mul_div stack={mul:?}");

        let product = run_vm(
            &format!(
                "use zoro::math\n\
                 begin\n\
                     push.{f}.{f}\n\
                     exec.math::sqrt_of_product\n\
                 end"
            ),
            &libs,
        )
        .expect("sqrt of product");
        let expected_product = isqrt(f as u128 * f as u128) as u64;
        assert_eq!(product[0], expected_product);
    }

    #[test]
    fn test_vm_swap_quote_and_lp() {
        let pool = get_pool_library().expect("pool library");
        let lp = get_lp_local_library().expect("lp library");
        let felt_max = felt_max();

        let near_out = largest_symmetric_product(997);
        let out_cases = [
            (near_out, near_out, near_out),
            (1, 1, 1),
            (1_000, 50_000, 50_000),
            (100_000, 1_000_000_000, 100_000_000_000),
        ];
        for (amount_in, reserve_in, reserve_out) in out_cases {
            let got = run_vm(
                &format!(
                    "use zoro::xyk_pool\n\
                     begin\n\
                         push.{reserve_out}.{reserve_in}.{amount_in}\n\
                         exec.xyk_pool::get_amount_out_u64\n\
                     end"
                ),
                &[pool.clone()],
            )
            .unwrap_or_else(|e| panic!("amount_out {amount_in},{reserve_in},{reserve_out}: {e}"));
            assert_eq!(
                got[0],
                get_amount_out(amount_in, reserve_in, reserve_out),
                "amount_out {amount_in},{reserve_in},{reserve_out}"
            );
        }

        let quote_cases = [
            (felt_max, felt_max, felt_max),
            (1, 1, 1),
            (1_000, 4_000, 9_000),
        ];
        for (amount, reserve_a, reserve_b) in quote_cases {
            let got = run_vm(
                &format!(
                    "use zoro::xyk_pool\n\
                     begin\n\
                         push.{reserve_b}.{reserve_a}.{amount}\n\
                         exec.xyk_pool::quote\n\
                     end"
                ),
                &[pool.clone()],
            )
            .unwrap_or_else(|e| panic!("quote {amount},{reserve_a},{reserve_b}: {e}"));
            let expected = amount as u128 * reserve_b as u128 / reserve_a as u128;
            assert_eq!(got[0], expected as u64, "quote {amount}");
        }

        let near_in = largest_symmetric_product(1000);
        let in_cases = [
            (near_in, near_in, near_in * 2),
            (1, 1, 2),
            (100, 50_000, 80_000),
        ];
        for (amount_out, reserve_in, reserve_out) in in_cases {
            let got = run_vm(
                &format!(
                    "use zoro::xyk_pool\n\
                     begin\n\
                         push.{reserve_out}.{reserve_in}.{amount_out}\n\
                         exec.xyk_pool::get_amount_in_u64\n\
                     end"
                ),
                &[pool.clone()],
            )
            .unwrap_or_else(|e| panic!("amount_in {amount_out},{reserve_in},{reserve_out}: {e}"));
            assert_eq!(
                got[0],
                get_amount_in(amount_out, reserve_in, reserve_out),
                "amount_in {amount_out},{reserve_in},{reserve_out}"
            );
        }

        let lp_cases = [
            (0, felt_max, felt_max, 0, 0),
            (felt_max, felt_max, felt_max, felt_max, felt_max),
            (0, 10_000, 10_000, 0, 0),
            (1_000_000, 5_000, 9_000, 50_000, 80_000),
            (1_000_000, 9_000, 1_000, 50_000, 80_000),
        ];
        for (total, amount_0, amount_1, reserve_0, reserve_1) in lp_cases {
            let got = run_vm(
                &format!(
                    "use zoro::lp_local\n\
                     begin\n\
                         push.{reserve_1}.{reserve_0}.{amount_1}.{amount_0}.{total}\n\
                         exec.lp_local::get_lp_amount_out\n\
                     end"
                ),
                &[lp.clone()],
            )
            .unwrap_or_else(|e| panic!("lp {total},{amount_0},{amount_1}: {e}"));
            assert_eq!(
                got[0],
                compute_expected_lp(amount_0, amount_1, reserve_0, reserve_1, total),
                "lp {total},{amount_0},{amount_1},{reserve_0},{reserve_1}"
            );
        }

        let withdraw = run_vm(
            &format!(
                "use zoro::lp_local\n\
                 begin\n\
                     push.{felt_max}.{felt_max}.{felt_max}.{felt_max}\n\
                     exec.lp_local::simulate_withdraw\n\
                 end"
            ),
            &[lp.clone()],
        )
        .expect("withdraw");
        let (amount_0, amount_1) =
            compute_expected_withdraw(felt_max, felt_max, felt_max, felt_max);
        assert_eq!(withdraw[0], amount_0);
        assert_eq!(withdraw[1], amount_1);

        let overflow = run_vm(
            &format!(
                "use zoro::xyk_pool\n\
                 begin\n\
                     push.{felt_max}.{felt_max}.{felt_max}\n\
                     exec.xyk_pool::get_amount_out_u64\n\
                 end"
            ),
            &[pool.clone()],
        );
        assert!(overflow.is_err(), "felt-max swap must overflow u128");
    }

    #[test]
    fn test_storage_fuzz_scripts_compile() {
        let add_source = "use zoro::storage_fuzz_dummy\n\
             #use zoro::storage_utils\n\
             use miden::core::sys\n
             const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n\
             const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n\
             begin\n\
                 push.42\n\
                 push.VALUE_SLOT[0..2]\n\
                 call.storage_fuzz_dummy::add_to_storage_item\n\
                 exec.sys::truncate_stack\n\
             end";
        let result = compile_storage_fuzz_tx_script(add_source);
        assert!(result.is_ok(), "compile error: {:?}", result.err());
    }
}
