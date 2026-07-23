use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "pcas",
    about = "Content-addressable storage for datasets and model weights"
)]
struct Cli {
    /// Override CAS root directory (default: $CAS_ROOT or ~/data/blob)
    #[arg(long)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add files from local paths to the CAS
    AddPath {
        /// Files to add
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Tags to apply to all added blobs (repeatable)
        #[arg(long = "tag", num_args = 1)]
        tags: Vec<String>,
        /// Metadata string to set on all added blobs
        #[arg(long = "meta")]
        meta: Option<String>,
    },
    /// Download a URL and store in CAS
    AddUrl {
        /// URL to download
        url: String,
        /// Expected SHA-256 hash (if provided, verifies after download)
        #[arg(long)]
        sha256: Option<String>,
        /// Extract zip archive and store each file individually
        #[arg(long)]
        unzip: bool,
    },
    /// Print the CAS path for a hash
    Path {
        /// SHA-256 hash
        hash: String,
    },
    /// Discover and index visible regular files under the CAS root
    Index {
        /// Optional glob pattern; without `/` matches basenames recursively,
        /// with `/` matches root-relative paths. Omit to index everything.
        pattern: Option<String>,
    },
    /// Package operations
    Pkg {
        #[command(subcommand)]
        command: PkgCommands,
    },
    /// Export a package to a directory
    Export {
        /// Package name
        package: String,
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
    /// Add tags to a blob or package
    Tag {
        /// Blob hash or package name
        id: String,
        /// Tags to add
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Set metadata string on a blob or package
    Meta {
        /// Blob hash or package name
        id: String,
        /// Metadata value
        value: String,
    },
    /// Add a relation between two blobs
    Rel {
        /// Source blob hash
        source: String,
        /// Target blob hash
        target: String,
        /// Optional note describing the relation
        note: Option<String>,
    },
    /// Run as a Git LFS custom transfer agent (stdin/stdout protocol)
    LfsAgent,
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

fn run_index(root: &Path, pattern: Option<&str>) -> anyhow::Result<()> {
    let report = purecas::index::index_root(root, pattern)?;
    for indexed in &report.created {
        println!(
            "{}  {}  {}",
            indexed.digest,
            indexed.relative_path,
            indexed.object_path.display()
        );
    }
    println!("{}", report.summary);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(cli.root)?;

    // `index` and `path` operate purely on the packed filesystem object
    // layout under `.pcas`; they must never open or create `purecas.db`.
    match cli.command {
        Commands::Index { pattern } => return run_index(&root, pattern.as_deref()),
        Commands::Path { hash } => {
            let path = purecas::index::resolve_digest_path(&root, &hash)?;
            println!("{}", path.display());
            return Ok(());
        }
        _ => {}
    }

    let store = purecas::Store::open(&root)?;

    match cli.command {
        Commands::Index { .. } | Commands::Path { .. } => unreachable!("handled above"),
        Commands::AddPath { files, tags, meta } => {
            for file in &files {
                let blob = store.add_path(file)?;
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
                if !tag_refs.is_empty() {
                    blob.add_tags(&tag_refs)?;
                }
                if let Some(ref m) = meta {
                    blob.set_metadata(m)?;
                }
                println!("{} {}", blob.hash(), name);
            }
            Ok(())
        }
        Commands::AddUrl { url, sha256, unzip } => {
            match (sha256, unzip) {
                (Some(hash), true) => {
                    let blobs = store.add_verified_url_unzip(&url, &hash)?;
                    for blob in &blobs {
                        let names = blob.names().unwrap_or_default();
                        let name = names.first().map(|s| s.as_str()).unwrap_or("");
                        println!("{} {}", blob.hash(), name);
                    }
                }
                (Some(hash), false) => {
                    let blob = store.add_verified_url(&url, &hash)?;
                    println!("{}", blob.hash());
                }
                (None, true) => {
                    let blobs = store.add_url_unzip(&url)?;
                    for blob in &blobs {
                        let names = blob.names().unwrap_or_default();
                        let name = names.first().map(|s| s.as_str()).unwrap_or("");
                        println!("{} {}", blob.hash(), name);
                    }
                }
                (None, false) => {
                    let blob = store.add_url(&url)?;
                    println!("{}", blob.hash());
                }
            }
            Ok(())
        }
        Commands::Pkg { command } => match command {
            PkgCommands::Create { name, description } => {
                store.create_package(&name, description.as_deref())?;
                println!("Created package: {}", name);
                Ok(())
            }
            PkgCommands::Add { name, hashes, path } => {
                if path.is_some() && hashes.len() > 1 {
                    anyhow::bail!("--path can only be used with a single hash");
                }
                let pkg = store.package(&name);
                for hash in &hashes {
                    let blob = store.blob(hash);
                    pkg.add_blob(&blob, path.as_deref())?;
                }
                Ok(())
            }
            PkgCommands::List => {
                let pkgs = store.list_packages()?;
                for pkg in &pkgs {
                    let blob_count = pkg.blobs().map(|b| b.len()).unwrap_or(0);
                    println!("{}\t{} blobs", pkg.name(), blob_count);
                }
                Ok(())
            }
            PkgCommands::Show { name } => {
                let pkg = store.package(&name);
                let blobs = pkg.blobs()?;
                for info in &blobs {
                    let path_str = info.path.as_deref().unwrap_or("-");
                    let names_str = if info.names.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", info.names.join(", "))
                    };
                    println!("{}\t{}{}", info.hash, path_str, names_str);
                }
                Ok(())
            }
            PkgCommands::Rm { name } => {
                store.package(&name).remove()?;
                println!("Removed package: {}", name);
                Ok(())
            }
        },
        Commands::Export { package, to } => {
            std::fs::create_dir_all(&to)?;
            store.package(&package).export(&to)?;
            println!("Exported package '{}' to {}", package, to.display());
            Ok(())
        }
        Commands::Import { from } => {
            let result = store.import(&from)?;
            println!(
                "Imported {} blob(s) from {}",
                result.imported_blobs,
                from.display()
            );
            Ok(())
        }
        Commands::Tag { id, tags } => {
            let blob = store.blob(&id);
            let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
            blob.add_tags(&tag_refs)?;
            let all_tags = blob.tags()?;
            println!("{}: {}", id, all_tags.join("; "));
            Ok(())
        }
        Commands::Meta { id, value } => {
            let blob = store.blob(&id);
            blob.set_metadata(&value)?;
            println!("{}: {}", id, value);
            Ok(())
        }
        Commands::Rel {
            source,
            target,
            note,
        } => {
            let src = store.blob(&source);
            let tgt = store.blob(&target);
            src.add_relation(&tgt, note.as_deref())?;
            match &note {
                Some(n) => println!("{} -> {} ({})", source, target, n),
                None => println!("{} -> {}", source, target),
            }
            Ok(())
        }
        Commands::LfsAgent => purecas::lfs::run_agent(&root),
    }
}
