//! `dmem setup`: an interactive first-run wizard. Detects installed agents, wires the hooks
//! you pick, optionally seeds a first memory, and (with the server feature) helps you set up
//! a server token. Needs a real terminal. Behind the `wizard` feature.

use anyhow::{anyhow, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect};

pub fn run() -> Result<()> {
    let theme = ColorfulTheme::default();
    println!("dmem setup\n");

    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let devin_present = home.join(".config/devin").exists();
    let claude_present = home.join(".claude").exists();
    println!(
        "Detected agents: Devin {}, Claude Code {}\n",
        if devin_present { "yes" } else { "no" },
        if claude_present { "yes" } else { "no" }
    );

    // 1. pick which agents to wire (pre-selecting the detected ones)
    let agents = ["Devin CLI", "Claude Code"];
    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Wire dmem into which agents? (space toggles, enter confirms)")
        .items(&agents)
        .defaults(&[devin_present, claude_present])
        .interact()?;
    let devin = chosen.contains(&0);
    let claude = chosen.contains(&1);
    if devin || claude {
        crate::bootstrap::run(devin, claude)?;
    } else {
        println!("(skipped agent wiring)");
    }

    // 2. seed a first memory so recall is not empty
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
            let uri = crate::tools::Memory::open()?.remember(&text, "resources/notes")?;
            println!("  stored {uri}");
        }
    }

    // 3. optional server token (only if this build has server mode)
    #[cfg(feature = "server")]
    if Confirm::with_theme(&theme)
        .with_prompt("Set up a token to run dmem as a memory server?")
        .default(false)
        .interact()?
    {
        let tenant: String = Input::with_theme(&theme)
            .with_prompt("Tenant name")
            .default("default".to_string())
            .interact_text()?;
        let token: String = dialoguer::Password::with_theme(&theme)
            .with_prompt("Bearer token (kept secret)")
            .interact()?;
        let envname = format!("DM_TOKEN_{}", tenant.to_uppercase());
        println!("\nTo start the server, export the token and run:");
        println!("  export {envname}={token}");
        println!("  dmem serve --addr 0.0.0.0:8077");
    }

    // 4. summary
    let data = crate::config::data_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into());
    println!("\nSetup complete.");
    println!("  data dir: {data}");
    println!("  save:   dmem remember \"...\"");
    println!("  recall: dmem recall \"...\"");
    Ok(())
}
