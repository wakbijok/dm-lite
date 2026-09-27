//! `dmem upgrade`: in-place self-update from GitHub Releases (wakbijok/dm-lite), with
//! MANDATORY minisign verification (security audit 11-08-2026, High #4). Two channels:
//! stable (default) and pre-release (`--pre`). Picks the newest release by semver, downloads
//! the target archive AND its detached `.minisig`, verifies the signature against the
//! release public key compiled into this binary, asks for confirmation (unless `--yes`),
//! then replaces the running binary (and the native vector lib when the archive carries a
//! newer one). An unsigned or tampered archive never touches disk outside the temp dir.
//!
//! Signing (release runbook): `contrib/sign-release.py <tag>` on the release machine. It
//! checks the release, asks for the key passphrase once, signs and verifies every asset,
//! and uploads the `.minisig` files. The secret key never leaves that machine.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::Path;

const OWNER: &str = "wakbijok";
const REPO: &str = "dm-lite";

/// The dm-lite release signing public keys (minisign format). Verification is NOT optional:
/// compromising the GitHub account is no longer enough to push code to upgraders - the
/// attacker would also need an offline secret key. A list, so a new key can ship (and be
/// trusted) before releases are signed with it. Each release still carries one signature, so
/// an install that skips the whole overlap window needs a manual reinstall.
/// 0.3.6: key 1C4494F1B9A9FD04 retired (its passphrase was lost), CD4359A138D28837 added.
const RELEASE_PUBKEYS: &[&str] = &["RWQ3iNI4oVlDzaiCJtUwwBiPjx2dcWaDHxh00Pw0lQbluZ1V33c0+gyR"];

/// Ok when any of `keys` verifies `sig_text` over `archive` AND the signed trusted comment
/// names `expected_file` (`file:<name>`, as the release signing script writes it). Without the
/// name check, a genuinely signed archive could be replayed under another version or target.
/// A key that fails to parse is skipped, never trusted; legacy (non-prehashed) signatures are
/// refused.
fn verify_release_sig(keys: &[&str], archive: &[u8], sig_text: &str, expected_file: &str) -> Result<()> {
    let sig = minisign_verify::Signature::decode(sig_text).map_err(|e| anyhow!("decode signature: {e}"))?;
    let mut last_err = anyhow!("no trusted release key");
    for k in keys {
        match minisign_verify::PublicKey::from_base64(k) {
            Ok(pk) => match pk.verify(archive, &sig, false) {
                Ok(()) => {
                    // The trusted comment is covered by the signature, so this check is authentic.
                    let want = format!("file:{expected_file}");
                    if sig.trusted_comment().split('\t').any(|f| f == want) {
                        return Ok(());
                    }
                    bail!("signature is for a different file (trusted comment: {})", sig.trusted_comment());
                }
                Err(e) => last_err = anyhow!("{e}"),
            },
            Err(e) => last_err = anyhow!("bad compiled-in release public key: {e}"),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::verify_release_sig;

    // Throwaway test key pair, not a release key. FIXTURE_SIG signs FIXTURE with FIXTURE_KEY.
    const FIXTURE: &[u8] = b"dm-lite release signature fixture\n";
    const FIXTURE_KEY: &str = "RWT+b3NoBHVhGotlhgZvMbNSUFnVOErxhyWm7VY494qtY1OKL9wdITti";
    const OTHER_KEY: &str = "RWRbObgKQoAhYxvfwilUhuTnV1CUQsEuRs0xPf79if1EMvh8weYLlRdW";
    const FIXTURE_SIG: &str = "untrusted comment: signature from rsign secret key\n\
        RUT+b3NoBHVhGkt5tFJFWYDXBcQaF/c5H7GTwdi0cGo9aKm0U24Yc2aDhVlY4CVY8UvIz+UeWVWtFfmRLe16I4/e6mgJDufVlQg=\n\
        trusted comment: timestamp:1790135640\tfile:fixture.bin\tprehashed\n\
        qLQchMvxIvkFZVr3RuCyaJSnGOQyVO0EN9a4bsM9JOvPaTT7X+/eWCrY4ucw1xq7f/3jt4t23EswjS+GyrOhDQ==\n";

    #[test]
    fn any_trusted_key_verifies() {
        verify_release_sig(&[FIXTURE_KEY], FIXTURE, FIXTURE_SIG, "fixture.bin").expect("signing key verifies");
        verify_release_sig(&[OTHER_KEY, FIXTURE_KEY], FIXTURE, FIXTURE_SIG, "fixture.bin").expect("second key in the list verifies");
        verify_release_sig(&["not a key", FIXTURE_KEY], FIXTURE, FIXTURE_SIG, "fixture.bin").expect("a bad entry is skipped");
    }

    #[test]
    fn untrusted_or_tampered_is_refused() {
        assert!(verify_release_sig(&[OTHER_KEY], FIXTURE, FIXTURE_SIG, "fixture.bin").is_err(), "wrong key");
        assert!(verify_release_sig(&[], FIXTURE, FIXTURE_SIG, "fixture.bin").is_err(), "no keys");
        assert!(verify_release_sig(&[FIXTURE_KEY], b"tampered", FIXTURE_SIG, "fixture.bin").is_err(), "tampered archive");
        let bad_comment = FIXTURE_SIG.replace("\tfile:", " file:");
        assert!(verify_release_sig(&[FIXTURE_KEY], FIXTURE, &bad_comment, "fixture.bin").is_err(), "altered trusted comment");
        // A valid signature replayed under another asset name (another version or target).
        assert!(verify_release_sig(&[FIXTURE_KEY], FIXTURE, FIXTURE_SIG, "other.bin").is_err(), "signed for another file");
        assert!(verify_release_sig(&[FIXTURE_KEY], FIXTURE, FIXTURE_SIG, "fixture").is_err(), "prefix of the signed name");
    }

    #[test]
    fn release_pubkeys_parse() {
        // If no key parses, every future `dmem upgrade` is bricked - catch it at test time,
        // not on a user's machine.
        assert!(!super::RELEASE_PUBKEYS.is_empty(), "at least one release key");
        for k in super::RELEASE_PUBKEYS {
            minisign_verify::PublicKey::from_base64(k).expect("compiled-in release key is valid");
        }
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
    // Exactly the archive the release workflow names for this tag and target; the signature
    // must name the same file (checked in verify_release_sig).
    let tag = release.get("tag_name").and_then(|v| v.as_str()).unwrap_or_default();
    let expected = format!("dmem-{tag}-{t}.tar.gz").to_lowercase();
    let (archive_name, archive_url) = asset_url(&|n| n == expected)
        .ok_or_else(|| anyhow!("release {latest} has no {expected} asset"))?;
    let (_, sig_url) = asset_url(&|n| n == format!("{}.minisig", archive_name.to_lowercase()))
        .ok_or_else(|| anyhow!("release {latest} has no {archive_name}.minisig - refusing an unsigned upgrade"))?;

    println!("downloading {archive_name} ({latest}, {t})...");
    let archive = download(&client, &archive_url)?;
    let sig_raw = download(&client, &sig_url)?;

    // Verify BEFORE anything is extracted or replaced.
    let sig_text = std::str::from_utf8(&sig_raw).context("signature is not UTF-8")?;
    verify_release_sig(RELEASE_PUBKEYS, &archive, sig_text, &archive_name)
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
