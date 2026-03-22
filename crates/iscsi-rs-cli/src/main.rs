use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "iscsi-rs", version, about = "macOS iSCSI initiator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover iSCSI targets on a portal
    Discover { portal: String },
    /// Login to an iSCSI target
    Login { target: String },
    /// Logout from an iSCSI target
    Logout { target: String },
    /// List active iSCSI sessions
    List,
    /// Show daemon and dext status
    Status,
    /// Activate the DriverKit system extension
    Activate,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Discover { portal } => println!("discover {portal}: not yet implemented"),
        Commands::Login { target } => println!("login {target}: not yet implemented"),
        Commands::Logout { target } => println!("logout {target}: not yet implemented"),
        Commands::List => println!("list: not yet implemented"),
        Commands::Status => println!("status: not yet implemented"),
        Commands::Activate => println!("activate: not yet implemented"),
    }
    Ok(())
}
