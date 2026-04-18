use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

mod db;
mod fetch;
mod lfs;
mod store;
mod transfer;

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
    /// Add files to the CAS
    Add {
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(cli.root)?;

    match cli.command {
        Commands::Add { files, tags, meta } => {
            let conn = db::open_db(&root)?;
            for file in &files {
                let hash = store::store_blob(&root, file)?;
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                db::insert_blob(&conn, &hash)?;
                if !name.is_empty() {
                    db::insert_blob_name(&conn, &hash, &name)?;
                }
                for tag in &tags {
                    db::add_tag(&conn, &hash, tag)?;
                }
                if let Some(ref m) = meta {
                    db::set_metadata(&conn, &hash, m)?;
                }
                println!("{} {}", hash, name);
            }
            Ok(())
        }
        Commands::Fetch { url, sha256, unzip } => {
            let conn = db::open_db(&root)?;
            if unzip {
                let results = fetch::fetch_unzip_and_store(&url, &sha256, &root)?;
                for (hash, name) in &results {
                    db::insert_blob(&conn, hash)?;
                    db::insert_blob_name(&conn, hash, name)?;
                    println!("{} {}", hash, name);
                }
            } else {
                let hash = fetch::fetch_and_store(&url, &sha256, &root)?;
                db::insert_blob(&conn, &hash)?;
                if let Some(name) = url.rsplit('/').next() {
                    if !name.is_empty() {
                        db::insert_blob_name(&conn, &hash, name)?;
                    }
                }
                println!("{}", hash);
            }
            Ok(())
        }
        Commands::Path { hash } => {
            let p = store::blob_path(&root, &hash);
            let status = if p.exists() { "[exists]" } else { "[missing]" };
            println!("{} {}", p.display(), status);
            Ok(())
        }
        Commands::Cat { hash } => store::cat_blob(&root, &hash),
        Commands::Pkg { command } => {
            let conn = db::open_db(&root)?;
            match command {
                PkgCommands::Create { name, description } => {
                    db::create_package(&conn, &name, description.as_deref())?;
                    println!("Created package: {}", name);
                    Ok(())
                }
                PkgCommands::Add { name, hashes, path } => {
                    if path.is_some() && hashes.len() > 1 {
                        anyhow::bail!("--path can only be used with a single hash");
                    }
                    for hash in &hashes {
                        db::add_blob_to_package(&conn, &name, hash, path.as_deref())?;
                    }
                    Ok(())
                }
                PkgCommands::List => {
                    let pkgs = db::list_packages(&conn)?;
                    for (name, count) in pkgs {
                        println!("{}\t{} blobs", name, count);
                    }
                    Ok(())
                }
                PkgCommands::Show { name } => {
                    let blobs = db::show_package(&conn, &name)?;
                    for (hash, path, names) in blobs {
                        let path_str = path.as_deref().unwrap_or("-");
                        let names_str = if names.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", names.join(", "))
                        };
                        println!("{}\t{}{}", hash, path_str, names_str);
                    }
                    Ok(())
                }
                PkgCommands::Rm { name } => {
                    db::remove_package(&conn, &name)?;
                    println!("Removed package: {}", name);
                    Ok(())
                }
            }
        }
        Commands::Export { targets, to } => {
            let conn = db::open_db(&root)?;
            fs::create_dir_all(&to)?;
            if targets.len() == 1 && db::package_exists(&conn, &targets[0])? {
                transfer::export_package(&conn, &root, &targets[0], &to)?;
                println!("Exported package '{}' to {}", targets[0], to.display());
            } else {
                transfer::export_hashes(&conn, &root, &targets, &to)?;
                println!("Exported {} blob(s) to {}", targets.len(), to.display());
            }
            Ok(())
        }
        Commands::Import { from } => {
            let conn = db::open_db(&root)?;
            let result = transfer::import_from(&conn, &root, &from)?;
            println!(
                "Imported {} blob(s) from {}",
                result.imported_blobs,
                from.display()
            );
            Ok(())
        }
        Commands::Tag { id, tags } => {
            let conn = db::open_db(&root)?;
            for tag in &tags {
                db::add_tag(&conn, &id, tag)?;
            }
            let all_tags = db::get_tags(&conn, &id)?;
            println!("{}: {}", id, all_tags.join("; "));
            Ok(())
        }
        Commands::Meta { id, value } => {
            let conn = db::open_db(&root)?;
            db::set_metadata(&conn, &id, &value)?;
            println!("{}: {}", id, value);
            Ok(())
        }
        Commands::Rel {
            source,
            target,
            note,
        } => {
            let conn = db::open_db(&root)?;
            db::add_relation(&conn, &source, &target, note.as_deref())?;
            match &note {
                Some(n) => println!("{} -> {} ({})", source, target, n),
                None => println!("{} -> {}", source, target),
            }
            Ok(())
        }
        Commands::LfsAgent => lfs::run_agent(&root),
    }
}
