//! `dmem upgrade`: in-place self-update from GitHub Releases (wakbijok/dm-lite), with
//! MANDATORY minisign verification (security audit 11-08-2026, High #4). Two channels:
//! stable (default) and pre-release (`--pre`). Picks the newest release by semver, downloads
//! the target archive AND its detached `.minisig`, verifies the signature against the
//! release public key compiled into this binary, asks for confirmation (unless `--yes`),
//! then replaces the running binary (and the native vector lib when the archive carries a
//! newer one). An unsigned or tampered archive never touches disk outside the temp dir.
//!
//! Signing (release runbook): `rsign sign -s <secret key> -x <asset>.minisig <asset>` for
//! every release asset. The secret key lives only on the release machine.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::Path;

const OWNER: &str = "wakbijok";
const REPO: &str = "dm-lite";

/// The dm-lite release signing public key (minisign format). Verification is NOT optional:
/// compromising the GitHub account is no longer enough to push code to upgraders - the
/// attacker would also need the offline secret key.
const RELEASE_PUBKEY: &str = "RWQE/am58ZREHA5bpSZ6y4IutRTO5G/AuojqEEysr0S1jnY/WucccMdE";

#[cfg(test)]
mod tests {
    #[test]
    fn release_pubkey_parses() {
        // If this key ever fails to parse, every future `dmem upgrade` is bricked - catch it
        // at test time, not on a user's machine.
        minisign_verify::PublicKey::from_base64(super::RELEASE_PUBKEY).expect("compiled-in release key is valid");
    }
}

/// The release-asset target triple for this build.
fn target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "unsupported"
    }
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("dmem/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()?)
}

fn download(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client.get(url).header("Accept", "application/octet-stream").send()?;
    if !resp.status().is_success() {
        bail!("download {url}: HTTP {}", resp.status());
    }
    let mut buf = Vec::new();
    resp.take(512 * 1024 * 1024).read_to_end(&mut buf)?; // hard cap: a release asset is ~tens of MB
    Ok(buf)
}

pub fn run(pre: bool, yes: bool) -> Result<()> {
    let channel = if pre { "pre-release" } else { "stable" };
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|e| anyhow!("parse current version: {e}"))?;
    if target() == "unsupported" {
        bail!("no release target for this platform; build from source instead");
    }

    let client = http()?;
    let releases: serde_json::Value = client
        .get(format!("https://api.github.com/repos/{OWNER}/{REPO}/releases?per_page=30"))
        .send()?
        .error_for_status()
        .context("fetch releases")?
        .json()?;

    // Newest release by semver for the channel (stable skips rc/beta pre-release versions).
    let mut best: Option<(semver::Version, &serde_json::Value)> = None;
    for r in releases.as_array().map(|a| a.as_slice()).unwrap_or_default() {
        let tag = r.get("tag_name").and_then(|t| t.as_str()).unwrap_or_default();
        let v = match semver::Version::parse(tag.trim_start_matches('v')) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !pre && !v.pre.is_empty() {
            continue;
        }
        if best.as_ref().map(|(bv, _)| v > *bv).unwrap_or(true) {
            best = Some((v, r));
        }
    }
    let Some((latest, release)) = best else {
        println!("no {channel} release found for {OWNER}/{REPO}");
        if !pre {
            println!("(try `dmem upgrade --pre` to include release candidates)");
        }
        return Ok(());
    };
    if latest <= current {
        println!("dmem {current} is already up to date (latest {channel}: {latest})");
        return Ok(());
    }

    // The archive asset for this target, and its detached signature. Missing signature =
    // hard refusal, not a downgrade to unsigned.
    let assets = release.get("assets").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    let asset_url = |name_pred: &dyn Fn(&str) -> bool| -> Option<(String, String)> {
        assets.iter().find_map(|a| {
            let name = a.get("name")?.as_str()?;
            if name_pred(&name.to_lowercase()) {
                Some((name.to_string(), a.get("browser_download_url")?.as_str()?.to_string()))
            } else {
                None
            }
        })
    };
    let t = target();
    let (archive_name, archive_url) = asset_url(&|n| n.contains(t) && n.ends_with(".tar.gz"))
        .ok_or_else(|| anyhow!("release {latest} has no .tar.gz asset for {t}"))?;
    let (_, sig_url) = asset_url(&|n| n == format!("{}.minisig", archive_name.to_lowercase()))
        .ok_or_else(|| anyhow!("release {latest} has no {archive_name}.minisig - refusing an unsigned upgrade"))?;

    println!("downloading {archive_name} ({latest}, {t})...");
    let archive = download(&client, &archive_url)?;
    let sig_raw = download(&client, &sig_url)?;

    // Verify BEFORE anything is extracted or replaced.
    let pk = minisign_verify::PublicKey::from_base64(RELEASE_PUBKEY)
        .map_err(|e| anyhow!("bad compiled-in release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(std::str::from_utf8(&sig_raw).context("signature is not UTF-8")?)
        .map_err(|e| anyhow!("decode {archive_name}.minisig: {e}"))?;
    pk.verify(&archive, &sig, false)
        .map_err(|e| anyhow!("SIGNATURE VERIFICATION FAILED for {archive_name}: {e} - refusing to install"))?;
    println!("signature verified (minisign, dm-lite release key)");

    if !yes {
        eprint!("install dmem {latest} over {current}? [y/N] ");
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        if !matches!(buf.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted; nothing changed");
            return Ok(());
        }
    }

    // Extract to a private temp dir, then swap the binary in place.
    let tmp = std::env::temp_dir().join(format!("dmem-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()))
        .unpack(&tmp)
        .context("extract release archive")?;
    let find = |name: &str| -> Option<std::path::PathBuf> {
        fn walk(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
            for e in std::fs::read_dir(dir).ok()?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Some(hit) = walk(&p, name) {
                        return Some(hit);
                    }
                } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
                    return Some(p);
                }
            }
            None
        }
        walk(&tmp, name)
    };
    let new_bin = find("dmem").ok_or_else(|| anyhow!("archive holds no `dmem` binary"))?;
    self_replace::self_replace(&new_bin).context("replace running binary")?;
    // The native vector lib rides in the same verified archive; refresh it beside the binary.
    if let Some(new_lib) = find("libzvec_c_api.so").or_else(|| find("libzvec_c_api.dylib")) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let dest = dir.join(new_lib.file_name().unwrap());
                let staged = dest.with_extension("so.new");
                if std::fs::copy(&new_lib, &staged).is_ok() && std::fs::rename(&staged, &dest).is_ok() {
                    println!("native vector lib refreshed");
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    println!("updated dmem {current} -> {latest}");
    Ok(())
}
