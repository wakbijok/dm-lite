//! `dmem setup`: an interactive first-run wizard. Picks local vs remote-client mode and
//! WRITES the config file, detects installed agents and wires the hooks you choose, and
//! optionally seeds a first memory. Needs a real terminal. Behind the `wizard` feature.

use anyhow::{anyhow, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect, Password, Select};

pub fn run() -> Result<()> {
    let theme = ColorfulTheme::default();
    println!("dmem setup\n");

    // 1. mode: local memory or connect to a remote server, then build the config file.
    let modes = ["Local memory (store on this machine)", "Connect to a memory server"];
    let mode = Select::with_theme(&theme)
        .with_prompt("How should this dmem store memory?")
        .items(&modes)
        .default(0)
        .interact()?;

    let mut cfg = String::new();
    if mode == 1 {
        let url: String = Input::with_theme(&theme)
            .with_prompt("Server URL (e.g. https://memory.myhost.tld:8077)")
            .interact_text()?;
        let token: String = Password::with_theme(&theme).with_prompt("Bearer token").interact()?;
        cfg.push_str(&format!("[server]\nurl = \"{}\"\ntoken = \"{}\"\n", url.trim(), token));
        if url.trim().starts_with("https://") {
            let insecure = Confirm::with_theme(&theme)
                .with_prompt("Accept a self-signed certificate (insecure)?")
                .default(false)
                .interact()?;
            if insecure {
                cfg.push_str("insecure = true\n");
            } else {
                let ca: String = Input::with_theme(&theme)
                    .with_prompt("Path to the server's cert/CA PEM (blank = system roots)")
                    .allow_empty(true)
                    .interact_text()?;
                if !ca.trim().is_empty() {
                    cfg.push_str(&format!("ca_cert = \"{}\"\n", ca.trim()));
                }
            }
        }
    } else {
        let tenant: String = Input::with_theme(&theme)
            .with_prompt("Tenant name")
            .default("default".to_string())
            .interact_text()?;
        cfg.push_str(&format!("tenant = \"{}\"\n", tenant.trim()));
    }

    let path = crate::config::config_path().ok_or_else(|| anyhow!("could not resolve a config dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &cfg)?;
    // the file may hold a token; lock it down on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("\nwrote config to {}", path.display());

    // 2. wire the agents you pick (hooks call this same binary, in whichever mode you chose)
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let devin_present = home.join(".config/devin").exists();
    let claude_present = home.join(".claude").exists();
    println!(
        "\nDetected agents: Devin {}, Claude Code {}",
        if devin_present { "yes" } else { "no" },
        if claude_present { "yes" } else { "no" }
    );
    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Wire dmem into which agents? (space toggles, enter confirms)")
        .items(&["Devin CLI", "Claude Code"])
        .defaults(&[devin_present, claude_present])
        .interact()?;
    let devin = chosen.contains(&0);
    let claude = chosen.contains(&1);
    if devin || claude {
        crate::bootstrap::run(devin, claude)?;
    } else {
        println!("(skipped agent wiring)");
    }

    // 3. seed a first memory (goes local or to the server, per the config just written)
    if Confirm::with_theme(&theme)
        .with_prompt("Seed a first memory now?")
        .default(true)
        .interact()?
    {
        let text: String = Input::with_theme(&theme)
            .with_prompt("Memory text")
            .allow_empty(true)
            .interact_text()?;
        if !text.trim().is_empty() {
            match crate::tools::Memory::open().and_then(|m| m.remember(&text, "resources/notes")) {
                Ok(uri) => println!("  stored {uri}"),
                Err(e) => eprintln!("  could not store ({e:#})"),
            }
        }
    }

    println!("\nSetup complete. Try:  dmem status   |   dmem recall \"...\"");
    Ok(())
}
