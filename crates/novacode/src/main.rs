use novacode_base::{logging, paths::Paths};

fn main() -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let _log = logging::init(&paths, std::env::var_os("NOVACODE_TRACE").is_some())?;
    tracing::info!(home = %paths.home().display(), "novacode starting");
    println!("novacode {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
