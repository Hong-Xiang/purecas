use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod store;
mod db;
mod fetch;
mod transfer;

#[derive(Parser)]
#[command(name = "pcas", about = "Content-addressable storage for datasets and model weights")]
struct Cli {
    /// Override CAS root directory (default: $CAS_ROOT or ~/data/blob)
    #[arg(long)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add files to the CAS
    Add {
        /// Files to add
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Fetch a URL, verify hash, and store in CAS
    Fetch {
        /// URL to download
        url: String,
        /// Expected SHA-256 hash
        #[arg(long)]
        sha256: String,
        /// Extract zip archive and store each file individually
        #[arg(long)]
        unzip: bool,
    },
    /// Print the CAS path for a hash
    Path {
        /// SHA-256 hash
        hash: String,
    },
    /// Output blob contents to stdout
    Cat {
        /// SHA-256 hash
        hash: String,
    },
    /// Package operations
    Pkg {
        #[command(subcommand)]
        command: PkgCommands,
    },
    /// Export blobs and metadata to a directory
    Export {
        /// Package name or blob hashes to export
        #[arg(required = true)]
        targets: Vec<String>,
        /// Destination directory
        #[arg(long)]
        to: PathBuf,
    },
    /// Import blobs and metadata from a directory
    Import {
        /// Source directory
        #[arg(long)]
        from: PathBuf,
    },
}

#[derive(Subcommand)]
enum PkgCommands {
    /// Create a new package
    Create {
        /// Package name
        name: String,
        /// Package description
        #[arg(long)]
        description: Option<String>,
    },
    /// Add blobs to a package
    Add {
        /// Package name
        name: String,
        /// Blob hashes
        #[arg(required = true)]
        hashes: Vec<String>,
        /// Logical path within the package (only valid with a single hash)
        #[arg(long)]
        path: Option<String>,
    },
    /// List all packages
    List,
    /// Show blobs in a package
    Show {
        /// Package name
        name: String,
    },
    /// Remove a package (blobs are kept)
    Rm {
        /// Package name
        name: String,
    },
}

fn resolve_root(cli_root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(root) = cli_root {
        return Ok(root);
    }
    if let Ok(root) = std::env::var("CAS_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join("data").join("blob"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(cli.root)?;

    match cli.command {
        Commands::Add { files } => todo!(),
        Commands::Fetch { url, sha256, unzip } => todo!(),
        Commands::Path { hash } => todo!(),
        Commands::Cat { hash } => todo!(),
        Commands::Pkg { command } => match command {
            PkgCommands::Create { name, description } => todo!(),
            PkgCommands::Add { name, hashes, path } => todo!(),
            PkgCommands::List => todo!(),
            PkgCommands::Show { name } => todo!(),
            PkgCommands::Rm { name } => todo!(),
        },
        Commands::Export { targets, to } => todo!(),
        Commands::Import { from } => todo!(),
    }
}
