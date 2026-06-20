//! `dmem upgrade`: in-place self-update from GitHub Releases (wakbijok/dm-lite).
//! Two channels: stable (default) and pre-release (`--pre`, includes rc/beta). Picks the
//! newest release by semver for the channel, then replaces the running binary. Data lives
//! in a separate dir and is never touched; the schema migrates on the next open.

use anyhow::{anyhow, Result};

const OWNER: &str = "wakbijok";
const REPO: &str = "dm-lite";

pub fn run(pre: bool) -> Result<()> {
    let channel = if pre { "pre-release" } else { "stable" };
    let target = self_update::get_target();
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|e| anyhow!("parse current version: {e}"))?;

    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()
        .map_err(|e| anyhow!("configure release list: {e}"))?
        .fetch()
        .map_err(|e| anyhow!("fetch releases: {e}"))?;

    // Pick the newest release by semver. Stable channel skips anything with a pre-release
    // component (rc/beta); the pre channel considers all.
    let mut best: Option<(semver::Version, String)> = None;
    for r in &releases {
        let ver_str = r.version.trim_start_matches('v');
        let v = match semver::Version::parse(ver_str) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !pre && !v.pre.is_empty() {
            continue;
        }
        if best.as_ref().map(|(bv, _)| v > *bv).unwrap_or(true) {
            best = Some((v, format!("v{ver_str}")));
        }
    }

    let (latest, tag) = match best {
        Some(x) => x,
        None => {
            println!("no {channel} release found for {OWNER}/{REPO}");
            if !pre {
                println!("(try `dmem upgrade --pre` to include release candidates)");
            }
            return Ok(());
        }
    };

    if latest <= current {
        println!("dmem {current} is already up to date (latest {channel}: {latest})");
        return Ok(());
    }

    println!("updating dmem {current} -> {latest} ({target})...");
    let status = self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("dmem")
        .target(target)
        .target_version_tag(&tag)
        .current_version(&current.to_string())
        .show_download_progress(true)
        .no_confirm(true)
        .build()
        .map_err(|e| anyhow!("configure update: {e}"))?
        .update()
        .map_err(|e| anyhow!("update: {e}"))?;

    println!("updated dmem to {}", status.version());
    // The release archive also carries the native vector lib; it is pinned (zvec v0.5.0) and
    // usually identical across dmem versions, so we replace only the binary here. If a future
    // release bumps the native lib, re-download the full archive.
    println!("(native vector lib unchanged; re-download the archive if a release bumps it)");
    Ok(())
}
