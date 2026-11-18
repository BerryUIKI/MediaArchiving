use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use mediaarchiving_core::{
    analyse, build_plan, human_bytes, report, scan, Inventory, Policy, RunDir, ScanOptions,
};

#[derive(Parser, Debug)]
#[command(name = "ma")]
#[command(about = "Curate a large media library: scan, analyse, plan, report")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Walk the library and build the inventory (resumable, never modifies files)
    Scan {
        /// Directory to scan (may be repeated)
        #[arg(required = true)]
        roots: Vec<PathBuf>,

        /// Run directory holding the inventory, policy, plan and report
        #[arg(short, long, default_value = "runs/default")]
        run: PathBuf,

        /// Re-probe every file even when size and mtime are unchanged
        #[arg(long)]
        force: bool,

        /// Skip duplicate detection (the hashing pass)
        #[arg(long)]
        no_hash: bool,
    },

    /// Analyse the inventory against the policy and write analysis + plan + report
    Plan {
        #[arg(short, long, default_value = "runs/default")]
        run: PathBuf,
    },

    /// Write a commented policy template if the run has none yet
    Init {
        #[arg(short, long, default_value = "runs/default")]
        run: PathBuf,
    },

    /// Print a summary of the inventory without re-scanning
    Status {
        #[arg(short, long, default_value = "runs/default")]
        run: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan {
            roots,
            run,
            force,
            no_hash,
        } => {
            for root in &roots {
                if !root.exists() {
                    bail!("scan root does not exist: {}", root.display());
                }
            }

            let run_dir = RunDir::new(&run);
            run_dir.ensure()?;
            let created = Policy::ensure_template(&run_dir.policy())?;
            if created {
                println!("wrote policy template: {}", run_dir.policy().display());
            }

            let inventory = Inventory::open(&run_dir.inventory())?;
            print!("Loading inventory… ");
            let before = inventory.count()?;
            println!("{before} files known\n");

            let options = ScanOptions {
                roots: roots.clone(),
                force,
                // Protect the run directory even when it sits inside a root.
                skip_paths: vec![std::path::absolute(&run_dir.root).unwrap_or(run_dir.root.clone())],
                ..ScanOptions::default()
            };
            println!("Walking {} root(s)…", roots.len());
            let summary = scan(&inventory, &options)?;
            println!(
                "  seen {} · new/changed {} · unchanged {} · pruned {} · errors {} · {:.1}s",
                summary.files_seen,
                summary.new_or_changed,
                summary.unchanged,
                summary.pruned,
                summary.errors,
                summary.elapsed_ms as f64 / 1000.0
            );

            if !no_hash {
                println!("\nHashing files that share a size with another file…");
                let hashed = mediaarchiving_core::scan::hash_size_collisions(&inventory)?;
                println!("  hashed {hashed} files");
                let stats = mediaarchiving_core::scan::group_duplicates(&inventory)?;
                println!(
                    "  {} duplicate groups · {} redundant files · {} wasted",
                    stats.groups,
                    stats.redundant_files,
                    human_bytes(stats.wasted_bytes)
                );
            }

            let total = inventory.count()?;
            println!("\nInventory now holds {total} files.");
            println!("Next: ma plan");
            println!("      edit {} first", run_dir.policy().display());
        }

        Commands::Plan { run } => {
            let run_dir = RunDir::new(&run);
            let inventory = Inventory::open(&run_dir.inventory())?;
            let total = inventory.count()?;
            if total == 0 {
                bail!(
                    "inventory at {} is empty — run `ma scan` first",
                    run_dir.inventory().display()
                );
            }

            let policy = Policy::load(&run_dir.policy())?;
            println!(
                "Analysing {total} files · intent {} · image {} · video {}",
                policy.intent.label(),
                policy.image.target,
                policy.video.target
            );

            let records = inventory.all()?;
            let analysis = analyse(&records, &policy);

            let t = &analysis.totals;
            println!("\n── Analysis ──");
            println!("  files                {}", t.files);
            println!("  total size           {}", human_bytes(t.total_bytes));
            println!(
                "  transcode candidates {} ({})",
                t.transcodable_files,
                human_bytes(t.transcodable_bytes)
            );
            println!(
                "  estimated saving     {} (estimate, not measured)",
                human_bytes(t.estimated_saving_bytes)
            );
            println!("  redundant duplicates {}", human_bytes(t.redundant_bytes));
            println!("  needs fixing first   {}", t.blocked_files);

            let plan = build_plan(&analysis);
            println!("\n── Plan ──");
            println!(
                "  {} batches · {} files · {} est. saving",
                plan.totals.batches,
                plan.totals.files,
                human_bytes(plan.totals.estimated_saving)
            );
            for batch in plan.batches.iter().take(15) {
                println!(
                    "    {:<34} {:<6} {:>6} files  {:>10}  {:>10} saved",
                    batch.id,
                    batch.risk.as_str(),
                    batch.files,
                    human_bytes(batch.bytes),
                    human_bytes(batch.estimated_saving)
                );
            }
            if plan.batches.len() > 15 {
                println!("    … and {} more batches", plan.batches.len() - 15);
            }

            run_dir.ensure()?;
            std::fs::write(run_dir.analysis(), serde_json::to_vec_pretty(&analysis)?)?;
            std::fs::write(run_dir.plan(), serde_json::to_vec_pretty(&plan)?)?;
            report::write_report(&run_dir, &analysis, &plan, &policy)?;

            println!("\nWrote:");
            println!("  {}", run_dir.analysis().display());
            println!("  {}", run_dir.plan().display());
            println!("  {}", run_dir.report().display());
            println!("\nNo files were modified.");
        }

        Commands::Init { run } => {
            let run_dir = RunDir::new(&run);
            run_dir.ensure()?;
            let created = Policy::ensure_template(&run_dir.policy())?;
            if created {
                println!("wrote {}", run_dir.policy().display());
            } else {
                println!("policy already exists: {}", run_dir.policy().display());
            }
        }

        Commands::Status { run } => {
            let run_dir = RunDir::new(&run);
            let inventory = Inventory::open(&run_dir.inventory())?;
            println!("run      {}", run_dir.root.display());
            println!("files    {}", inventory.count()?);
            if let Ok(records) = inventory.all() {
                let bytes: u64 = records.iter().map(|r| r.size).sum();
                println!("size     {}", human_bytes(bytes));
            }
            println!(
                "policy   {}",
                if run_dir.policy().exists() {
                    "present"
                } else {
                    "missing"
                }
            );
            println!(
                "report   {}",
                if run_dir.report().exists() {
                    "present"
                } else {
                    "missing"
                }
            );
        }
    }

    Ok(())
}
