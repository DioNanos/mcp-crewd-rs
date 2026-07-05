//! `crew` binary — operator CLI (SPEC §15: `status`, `inspect <id>`,
//! `audit verify`) and the per-cell MCP shim (`crew mcp`, Task 14). Operator
//! subcommands authenticate to `crewd` as a `read_audit`-capable cell; `mcp`
//! authenticates as the cell it fronts.
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "crew", version, about = "mcp-crewd-rs operator + cell shim")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon health snapshot (SPEC §15 `crew status`).
    Status {
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        cell: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Envelope + ask + matching audit events for a message_id/ask_id.
    Inspect {
        id: String,
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        cell: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Audit chain operations (SPEC §15 `crew audit verify`).
    Audit {
        #[command(subcommand)]
        audit: AuditCmd,
    },
    /// Run the per-cell MCP stdio shim (Task 14).
    Mcp {
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        cell: String,
        #[arg(long)]
        token_file: PathBuf,
        /// Worker mode (SPEC §20.7 nesting OFF): hide the spawn surface
        /// (`cell_spawn`/`cell_send_task`/`cell_cancel`) — used when this shim
        /// runs inside a cell that is itself a worker.
        #[arg(long, default_value_t = false)]
        worker_mode: bool,
    },
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Walk the audit hash chain end-to-end.
    Verify {
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        cell: String,
        #[arg(long)]
        token_file: PathBuf,
    },
}

fn read_token(token_file: &PathBuf) -> Result<String, String> {
    std::fs::read_to_string(token_file)
        .map(|t| t.trim().to_string())
        .map_err(|e| format!("read token: {e}"))
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Mcp {
            runtime_dir,
            cell,
            token_file,
            worker_mode,
        } => {
            let token = match read_token(&token_file) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            let reader = tokio::io::BufReader::new(stdin);
            if let Err(e) =
                crew::mcp_shim::serve(reader, stdout, runtime_dir, cell, token, worker_mode).await
            {
                eprintln!("{e}");
                return std::process::ExitCode::FAILURE;
            }
            std::process::ExitCode::SUCCESS
        }
        other => {
            let res = match other {
                Cmd::Status {
                    runtime_dir,
                    cell,
                    token_file,
                } => match read_token(&token_file) {
                    Ok(t) => crew::ops::status(&runtime_dir, &cell, &t).await,
                    Err(e) => Err(e),
                },
                Cmd::Inspect {
                    id,
                    runtime_dir,
                    cell,
                    token_file,
                } => match read_token(&token_file) {
                    Ok(t) => crew::ops::inspect(&runtime_dir, &cell, &t, &id).await,
                    Err(e) => Err(e),
                },
                Cmd::Audit {
                    audit: AuditCmd::Verify { runtime_dir, cell, token_file },
                } => match read_token(&token_file) {
                    Ok(t) => crew::ops::audit_verify(&runtime_dir, &cell, &t).await,
                    Err(e) => Err(e),
                },
                Cmd::Mcp { .. } => unreachable!(),
            };
            match res {
                Ok(out) => {
                    println!("{out}");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}
