//! Read-only `world/` framing census for issue #367.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use orrery_persistd::{scan_world_census_fdb, FdbContext, DEFAULT_PAGE_ROWS};

/// Command-line arguments for the read-only world census.
#[derive(Debug, Parser)]
#[command(
    name = "world-census",
    about = "Read-only census of world/ component-bag framing"
)]
struct Cli {
    /// FoundationDB cluster file to inspect.
    #[arg(long, env = "ORRERY_FDB_CLUSTER_FILE")]
    fdb_cluster_file: PathBuf,

    /// Maximum world rows read in one FoundationDB transaction.
    #[arg(long, default_value_t = DEFAULT_PAGE_ROWS, env = "ORRERY_PAGE_ROWS")]
    page_rows: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cluster_file = cli.fdb_cluster_file.display().to_string();
    let context = FdbContext::connect(&cluster_file).context("open FoundationDB cluster")?;
    let census = scan_world_census_fdb(&context, cli.page_rows)
        .await
        .map_err(anyhow::Error::msg)?;

    println!("world census (read-only)");
    for (grid, counts) in census.grids {
        println!(
            "grid {grid}: framed={} legacy={}",
            counts.framed, counts.legacy
        );
        for (floor, rows) in counts.schema_floors {
            println!("  schema_floor {floor}: {rows}");
        }
    }
    println!(
        "non_live={} malformed_keys={}",
        census.non_live, census.malformed_keys
    );
    Ok(())
}
