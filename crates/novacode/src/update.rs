//! `novacode update` (L11.1): self-update for release installs only. How the binary
//! was installed is fixed at build time by `NOVACODE_INSTALL` (`release`, `aur`;
//! unset = a source build); a package manager's or a source build's binary is never
//! overwritten. An update never touches `~/.novacode`.

use sha2::{Digest, Sha256};
use std::process::ExitCode;

/// `owner/name` of the GitHub repo that publishes releases.
pub const REPO: &str = "Novaturiente/novacode";

pub fn install_kind() -> &'static str {
    option_env!("NOVACODE_INSTALL").unwrap_or("source")
}

/// This machine's release target, as named in release assets.
fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// `v1.2.3` / `1.2.3` → (1, 2, 3).
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut p = v.trim().trim_start_matches('v').split('.').map(|n| n.parse().ok());
    Some((p.next()??, p.next()??, p.next()??))
}

/// The hex SHA-256 from a `sha256sum`-style line must match `data`.
fn verify(data: &[u8], sums: &str) -> Result<(), String> {
    let want = sums.split_whitespace().next().unwrap_or_default().to_lowercase();
    let got = format!("{:x}", Sha256::digest(data));
    if want == got {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch (expected {want}, got {got}); nothing changed"
        ))
    }
}

/// The tag of the release repo's latest GitHub release.
async fn latest_tag(http: &reqwest::Client) -> anyhow::Result<String> {
    let latest: serde_json::Value = serde_json::from_str(
        &http
            .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?,
    )?;
    Ok(latest["tag_name"].as_str().unwrap_or_default().to_string())
}

/// Release `tag` is a newer version than `current`.
pub fn newer(tag: &str, current: &str) -> bool {
    matches!((parse(tag), parse(current)), (Some(n), Some(c)) if n > c)
}

/// L11.13: the latest release tag when it is newer than this binary, for every
/// install kind; an error when GitHub can't be asked (10 s timeout).
pub async fn check() -> anyhow::Result<Option<String>> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("novacode/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let tag = latest_tag(&http).await?;
    Ok(newer(&tag, env!("CARGO_PKG_VERSION")).then_some(tag))
}

pub async fn run() -> anyhow::Result<ExitCode> {
    match install_kind() {
        "release" => {}
        "aur" => {
            println!("novacode was installed from the AUR; update it with your AUR helper (e.g. `paru -Syu`).");
            return Ok(ExitCode::SUCCESS);
        }
        _ => {
            println!(
                "this is a source build; update with `git pull && cargo install --locked --path crates/novacode` in your checkout, or rerun `cargo install --locked --force --git https://github.com/Novaturiente/novacode novacode`."
            );
            return Ok(ExitCode::SUCCESS);
        }
    }
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    // A release binary repackaged by a package manager (novacode-bin) belongs to it.
    if exe.starts_with("/usr") {
        println!(
            "{} belongs to your package manager; update novacode with it.",
            exe.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let target = target().ok_or_else(|| anyhow::anyhow!("no release build for this platform"))?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("novacode/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let tag = &latest_tag(&http).await?;
    let (Some(new), Some(cur)) = (parse(tag), parse(env!("CARGO_PKG_VERSION"))) else {
        anyhow::bail!("can't read the latest release's version `{tag}`");
    };
    if new <= cur {
        println!("novacode {} is the newest release", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let asset = format!("novacode-{}-{target}.tar.gz", tag.trim_start_matches('v'));
    let base = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    println!("downloading {asset}");
    let data = http.get(&base).send().await?.error_for_status()?.bytes().await?;
    let sums = http
        .get(format!("{base}.sha256"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    verify(&data, &sums).map_err(anyhow::Error::msg)?;
    // Unpack next to the running binary, then swap it in with one rename.
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no folder for {}", exe.display()))?;
    let tmp = dir.join(format!(".novacode-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let tarball = tmp.join(&asset);
    std::fs::write(&tarball, &data)?;
    let ok = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&tmp)
        .status()?
        .success();
    let new_bin = tmp.join("novacode");
    if !ok || !new_bin.is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!("the release archive has no `novacode` binary; nothing changed");
    }
    std::fs::rename(&new_bin, &exe)?;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("updated to {tag}; running sessions keep the old version until /restart");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_checksums() {
        assert_eq!(parse("v1.10.2"), Some((1, 10, 2)));
        assert!(parse("v0.2.0") > parse("0.1.9"));
        assert_eq!(parse("nightly"), None);
        assert!(newer("v0.2.6", "0.2.3") && !newer("v0.2.3", "0.2.3") && !newer("nightly", "0.2.3"));
        let sum = format!("{:x}  novacode.tar.gz\n", Sha256::digest(b"abc"));
        assert!(verify(b"abc", &sum).is_ok());
        assert!(verify(b"abd", &sum).unwrap_err().contains("mismatch"));
        assert_eq!(install_kind(), "source", "test builds don't set NOVACODE_INSTALL");
    }
}
