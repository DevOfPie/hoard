use anyhow::{bail, Result};
use clap::Subcommand;

use hoard_agent::config::CliConfig;
use hoard_agent::prefs::Prefs;

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Create a default config file at the standard location
    Init {
        /// Server URL to write into the new config
        #[arg(long, default_value = "http://localhost:12421")]
        server: String,
        /// Overwrite an existing config without asking
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved config
    Show,
    /// Set a config field. Supported: server.url, updates.prerelease (true|false)
    Set { key: String, value: String },
    /// Print the path of the config file (whether it exists or not)
    Path,
}

pub async fn run(cmd: ConfigCommand) -> Result<()> {
    match cmd {
        ConfigCommand::Init { server, force } => {
            let path = CliConfig::default_path()?;
            if path.exists() && !force {
                bail!(
                    "config already exists at {} (pass --force to overwrite)",
                    path.display()
                );
            }
            let cfg = CliConfig {
                server: hoard_agent::config::ServerSection {
                    url: hoard_agent::serverclass::normalize_server_url(&server),
                },
                auth: Default::default(),
            };
            cfg.save(&path)?;
            println!("wrote {}", path.display());
        }
        ConfigCommand::Show => {
            let (cfg, path) = CliConfig::load_default()?;
            println!("# {}", path.display());
            print!("{}", toml::to_string_pretty(&cfg)?);
            // The update channel lives in the prefs the app shares, not in
            // config.toml; shown here because `config set` is where it is set.
            let (prefs, prefs_path) = Prefs::load_default()?;
            println!("\n# {}", prefs_path.display());
            println!("[updates]");
            println!("prerelease = {}", prefs.prerelease_updates);
        }
        ConfigCommand::Set { key, value } if key == "updates.prerelease" => {
            set_prerelease(&value).await?;
        }
        ConfigCommand::Set { key, value } => {
            let path = CliConfig::default_path()?;
            let mut cfg = CliConfig::load(&path)?;
            match key.as_str() {
                // A `user@` here would end up as an HTTP Basic header that
                // shadows the access key on every request: a 401 that blames the
                // token. Clean it on the way in, so what lands in config.toml is
                // what the client will actually talk to.
                "server.url" => {
                    cfg.server.url = hoard_agent::serverclass::normalize_server_url(&value)
                }
                other => bail!("unknown key: {other} (supported: server.url, updates.prerelease)"),
            }
            cfg.save(&path)?;
            println!("updated {}", path.display());
        }
        ConfigCommand::Path => {
            println!("{}", CliConfig::default_path()?.display());
        }
    }
    Ok(())
}

/// `hoard config set updates.prerelease true|false`: opt this machine in or out
/// of pre-release updates (HRD-D-0024).
///
/// It writes the same `prefs.json` the app's Settings toggle writes
/// (`<state_dir>/prefs.json`, [`Prefs::default_path`]), so the two can never
/// disagree, then tells a running service to check now. With no service there
/// is nobody to tell: it reads the preference when it next checks.
async fn set_prerelease(value: &str) -> Result<()> {
    let enabled = parse_bool(value)
        .ok_or_else(|| anyhow::anyhow!("updates.prerelease takes true or false, not {value:?}"))?;
    let path = Prefs::default_path()?;
    let mut prefs = Prefs::load_strict(&path)?;
    let changed = prefs.prerelease_updates != enabled;
    prefs.prerelease_updates = enabled;
    prefs.save(&path)?;
    println!("updated {}", path.display());
    if enabled {
        println!("pre-releases are test builds; this machine now updates to them too.");
    } else {
        println!("full releases only. Nothing is downgraded: a pre-release stays until a newer release ships.");
    }
    if changed && crate::commands::link::recheck_update().await {
        println!("the sync service is checking for updates now");
    }
    Ok(())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerelease_takes_a_boolean() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool(" FALSE "), Some(false));
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
