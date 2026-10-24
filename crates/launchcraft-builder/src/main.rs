use std::path::PathBuf;
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use launchcraft_builder::{BuildConfig, PackBuilder};

#[derive(Parser)]
#[command(name = "launchcraft-builder")]
#[command(about = "Headless modpack builder toolchain for Launchcraft CAS distribution", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a modpack directory into CAS blobs and manifest.json
    Build {
        /// Source directory containing mods, configs, and assets
        #[arg(short, long, default_value = "./pack")]
        input: PathBuf,

        /// Output directory where manifest.json and cas/ blobs will be saved
        #[arg(short, long, default_value = "./dist")]
        output: PathBuf,

        /// Optional path to pack.json / build configuration
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Pack ID override
        #[arg(long)]
        pack_id: Option<String>,

        /// Pack Version override
        #[arg(long)]
        pack_version: Option<String>,

        /// Game Version override
        #[arg(long)]
        game_version: Option<String>,
    },
    /// Initialize a new default pack configuration template
    Init {
        /// Path where the configuration file should be written
        #[arg(short, long, default_value = "./pack.json")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Build {
            input,
            output,
            config,
            pack_id,
            pack_version,
            game_version,
        } => {
            let mut build_config = BuildConfig::load_or_default(config.as_deref());

            if let Some(id) = pack_id {
                build_config.pack_id = id;
            }
            if let Some(v) = pack_version {
                build_config.version = v;
            }
            if let Some(gv) = game_version {
                build_config.game_version = gv;
            }

            let builder = PackBuilder::new(input, output, build_config);
            let manifest = builder.build()?;
            println!(
                "Successfully built pack '{}' v{} with {} files.",
                manifest.pack_id,
                manifest.pack_version,
                manifest.files.len()
            );
        }
        Commands::Init { path } => {
            let default_cfg = BuildConfig::default();
            let json = serde_json::to_string_pretty(&default_cfg)?;
            std::fs::write(&path, json)?;
            println!("Created default build configuration at {}", path.display());
        }
    }

    Ok(())
}
