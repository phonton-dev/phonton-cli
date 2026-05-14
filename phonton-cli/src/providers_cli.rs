use anyhow::{anyhow, Result};
use phonton_providers::{fetch_models_dev_catalog, probe_diff_contract, provider_for};
use serde::Serialize;

use crate::config;

#[derive(Serialize)]
struct ProviderListRow {
    id: String,
    name: String,
    route: String,
    models: usize,
    env: Vec<String>,
}

pub async fn run(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]).await,
        Some("sync") => sync().await,
        Some("doctor") => doctor(&args[1..]).await,
        Some("import-opencode") => import_opencode(&args[1..]),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_help();
            Ok(0)
        }
        Some(other) => Err(anyhow!("unknown providers subcommand `{other}`")),
    }
}

async fn list(args: &[String]) -> Result<i32> {
    let json = args.iter().any(|a| a == "--json");
    let catalog = match crate::load_cached_models_dev_catalog() {
        Some(catalog) => catalog,
        None => fetch_models_dev_catalog().await?,
    };
    if json {
        let rows: Vec<ProviderListRow> = catalog
            .iter()
            .map(|p| ProviderListRow {
                id: p.id.clone(),
                name: p.name.clone(),
                route: format!("{:?}", p.route_kind),
                models: p.models.len(),
                env: p.env.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("Models.dev providers ({}):", catalog.len());
        for p in catalog.iter().take(80) {
            println!(
                "{:<24} {:<22} {:>4} models  {:?}",
                p.id,
                p.name.chars().take(22).collect::<String>(),
                p.models.len(),
                p.route_kind
            );
        }
        if catalog.len() > 80 {
            println!("... {} more", catalog.len() - 80);
        }
    }
    Ok(0)
}

async fn sync() -> Result<i32> {
    let catalog = fetch_models_dev_catalog().await?;
    let path = crate::models_dev_catalog_cache_path()
        .ok_or_else(|| anyhow!("could not determine ~/.phonton path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
    println!(
        "Synced {} providers from Models.dev to {}",
        catalog.len(),
        path.display()
    );
    Ok(0)
}

async fn doctor(args: &[String]) -> Result<i32> {
    let configured = args.iter().any(|a| a == "--configured");
    let canary = args
        .windows(2)
        .any(|w| w[0] == "--canary" && w[1] == "diff")
        || args.iter().any(|a| a == "--canary=diff");
    if !configured {
        println!("providers doctor currently checks the configured provider; pass --configured.");
    }
    let cfg = config::load()?;
    let key = crate::provider_key_for_run(&cfg.provider)
        .ok_or_else(|| anyhow!("provider `{}` has no API key", cfg.provider.name))?;
    let model = cfg
        .provider
        .model
        .clone()
        .unwrap_or_else(|| crate::default_model_for(&cfg.provider.name));
    let provider_cfg = crate::make_api_provider_config(
        &cfg.provider.name,
        key,
        model.clone(),
        cfg.provider.account_id.clone(),
        cfg.provider.base_url.clone(),
    )
    .ok_or_else(|| anyhow!(crate::provider_config_failure_message(&cfg.provider.name)))?;
    if canary {
        let resp = probe_diff_contract(provider_for(provider_cfg).as_ref()).await?;
        println!(
            "✓ configured provider passed diff canary: {} via {}",
            resp.model_name, resp.provider
        );
    } else {
        println!(
            "configured provider is callable as {} / {}; add --canary diff to verify worker output",
            cfg.provider.name, model
        );
    }
    Ok(0)
}

fn import_opencode(args: &[String]) -> Result<i32> {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    if !dry_run {
        return Err(anyhow!(
            "import-opencode is read-only in v0.12.5; pass --dry-run to inspect available keys"
        ));
    }
    let path = config::opencode_auth_path()
        .ok_or_else(|| anyhow!("could not determine OpenCode auth path"))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("could not read {}: {e}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let providers = ["opencode", "opencode-go"];
    println!("OpenCode auth: {}", path.display());
    for provider in providers {
        let present = config::opencode_api_key_from_value(provider, &v).is_some();
        println!(
            "{provider}: {}",
            if present { "key found" } else { "not found" }
        );
    }
    println!("No secrets were copied or printed.");
    Ok(0)
}

fn print_help() {
    println!(
        "phonton providers\n\n\
         USAGE:\n  \
         phonton providers list [--json]\n  \
         phonton providers sync\n  \
         phonton providers doctor [--configured] [--canary diff]\n  \
         phonton providers import-opencode --dry-run"
    );
}
