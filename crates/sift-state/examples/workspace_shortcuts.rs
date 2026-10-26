//! Restore missing numbered workspace bindings without replacing custom keys.
//! cargo run -p sift-state --example workspace_shortcuts -- /path/to/config.toml
fn main() -> Result<(), String> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("Pass a config.toml path")?;
    let count = sift_state::Config::save_workspace_shortcuts(std::path::Path::new(&path))?;
    println!("Added {count} workspace bindings; existing bindings were kept.");
    Ok(())
}
