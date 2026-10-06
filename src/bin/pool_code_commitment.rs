use xyk_pool::utils::get_pool_account_code_commitment;

fn main() {
    let hash = get_pool_account_code_commitment();
    println!("Pool code commitment: {hash:?}");
    println!("Pool code commitment (hex): {}", hash.to_hex());
}
