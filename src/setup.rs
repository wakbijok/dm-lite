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

    // 2. name the AI and the user, then set up the default persona + governance (these are
    //    aliases - the persona identity - separate from the tenant/account).
    let agent_name: String = Input::with_theme(&theme)
        .with_prompt("What should I call your AI?")
        .default("Assistant".to_string())
        .interact_text()?;
    let user_name: String = Input::with_theme(&theme)
        .with_prompt("And your name (who am I working with)?")
        .allow_empty(true)
        .interact_text()?;
    let user_name = if user_name.trim().is_empty() { "you".to_string() } else { user_name.trim().to_string() };

    if Confirm::with_theme(&theme)
        .with_prompt("Set up the default persona and governance now?")
        .default(true)
        .interact()?
    {
        match crate::tools::Memory::open() {
            Ok(m) => {
                let persona_body = format!(
                    "I am {agent_name}, {user_name}'s collaborative partner. We share one memory \
                     across the tools {user_name} uses; it is our work, so I say \"we\".\n\n\
                     ## Voice\nDirect, concise, technical. I challenge a weak plan, then commit.\n\n\
                     ## What I do not do\nFiller openers, hedging when I know the answer, inventing \
                     past context.\n\n## Boundaries\nNever read or exfiltrate secrets; persist durable \
                     memory only through this system."
                );
                let title = format!("{agent_name} for {user_name}");
                match m.import_record(crate::entry::Kind::Persona, "agent/persona", &title, &persona_body) {
                    Ok(_) => println!("  persona set: {agent_name} working with {user_name}"),
                    Err(e) => eprintln!("  persona not set ({e:#})"),
                }
                for tpl in [
                    include_str!("../templates/save-discipline.md"),
                    include_str!("../templates/behavioral-discipline.md"),
                ] {
                    if let Ok((kind, ns, t, body)) = crate::entry::parse_frontmatter(tpl) {
                        if m.import_record(kind, &ns, &t, &body).is_ok() {
                            println!("  governance: {t}");
                        }
                    }
                }
            }
            Err(e) => eprintln!("  could not open memory to set persona ({e:#})"),
        }
    }

    // 3. wire the agents you EXPLICITLY pick. Nothing is pre-selected - wiring an agent that
    //    already has its own memory system (e.g. a Claude Code daimon-memory plugin) would
    //    double-inject, so the user must opt in, and we flag agents that look already-wired.
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let devin_cfg = home.join(".config/devin/config.json");
    let claude_cfg = home.join(".claude/settings.json");
    let codex_cfg = home.join(".codex/config.toml");
    let hermes_cfg = home.join(".hermes/config.yaml");
    let devin_present = home.join(".config/devin").exists();
    let claude_present = home.join(".claude").exists();
    let codex_present = codex_cfg.exists();
    let hermes_present = hermes_cfg.exists();
    // Devin/Claude use the JSON SessionStart-hook probe; Codex/Hermes have no such shape, so a
    // plain text probe flags a config that already references a daimon/dmem memory wiring.
    let json_label = |present: bool, cfg: &std::path::Path, name: &str| -> String {
        if !present {
            format!("{name} (not installed)")
        } else if crate::bootstrap::has_memory_hooks(cfg) {
            format!("{name} (already has memory hooks - leave unchecked unless replacing)")
        } else {
            name.to_string()
        }
    };
    let plain_label = |present: bool, cfg: &std::path::Path, name: &str| -> String {
        let wired = std::fs::read_to_string(cfg).map(|s| s.contains("daimon") || s.contains("dmem")).unwrap_or(false);
        if !present {
            format!("{name} (not installed)")
        } else if wired {
            format!("{name} (already references daimon/dmem - leave unchecked unless replacing)")
        } else {
            name.to_string()
        }
    };
    let items = [
        json_label(devin_present, &devin_cfg, "Devin CLI"),
        json_label(claude_present, &claude_cfg, "Claude Code"),
        plain_label(codex_present, &codex_cfg, "Codex"),
        plain_label(hermes_present, &hermes_cfg, "Hermes"),
    ];
    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Wire dmem into which agents? (nothing is pre-selected; space toggles, enter confirms)")
        .items(&items)
        .defaults(&[false, false, false, false])
        .interact()?;
    let devin = chosen.contains(&0);
    let claude = chosen.contains(&1);
    let codex = chosen.contains(&2);
    let hermes = chosen.contains(&3);
    if devin || claude || codex || hermes {
        crate::bootstrap::run(devin, claude, codex, hermes)?;
    } else {
        println!("(skipped agent wiring - undo any wiring later with `dmem bootstrap --remove`)");
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
