//! `ghostwritin`: rewrite files in a voice, locally.
//!
//! ```text
//! ghostwritin rewrite draft.md -o final.md --voice casual --strength edit
//! ghostwritin rewrite - --voice professional < draft.txt > final.txt
//! ghostwritin voice learn a.md b.md c.md -o style.txt
//! ghostwritin rewrite draft.md --voice my_voice --style style.txt
//! ```

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ghostwritin_cli::{NO_MODEL, model_from_env};
use ghostwritin_core::{DiffOp, GhostwritinError, RewriteRequest, Strength, StyleSummary, Voice};
use ghostwritin_rewrite::Engine;
use ghostwritin_voice::VoiceBuilder;

#[derive(Parser)]
#[command(
    name = "ghostwritin",
    version,
    about = "Rewrite drafts in your own voice; names, numbers, quotes and code stay locked."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Rewrite a Markdown or text file. Code fences, headings and tables are kept as they are.
    Rewrite {
        /// The draft, or `-` for standard input.
        input: PathBuf,
        /// Where to write the rewrite (default: standard output).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// casual, professional, academic or `my_voice`.
        #[arg(long, default_value = "professional", value_parser = parse_voice)]
        voice: Voice,
        /// polish, edit or rewrite.
        #[arg(long, default_value = "edit", value_parser = parse_strength)]
        strength: Strength,
        /// A style summary file (from `ghostwritin voice learn`), for `--voice my_voice`.
        #[arg(long)]
        style: Option<PathBuf>,
        /// Print the word diff to standard error.
        #[arg(long)]
        diff: bool,
        /// Write the full JSON answer (rewrite, scores, diff, locks) instead of the text.
        #[arg(long)]
        json: bool,
    },
    /// My voice.
    Voice {
        #[command(subcommand)]
        command: VoiceCommand,
    },
}

#[derive(Subcommand)]
enum VoiceCommand {
    /// Build a style summary from 3 to 5 samples of your own writing. Only the summary is written.
    Learn {
        samples: Vec<PathBuf>,
        /// Where to write the summary.
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn parse_voice(name: &str) -> Result<Voice, String> {
    Voice::parse(name)
        .ok_or_else(|| "expected casual, professional, academic or my_voice".to_owned())
}

fn parse_strength(name: &str) -> Result<Strength, String> {
    Strength::parse(name).ok_or_else(|| "expected polish, edit or rewrite".to_owned())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ghostwritin: {message}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &Path) -> Result<String, String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("reading standard input: {e}"))?;
        return Ok(text);
    }
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn write(path: Option<&Path>, text: &str) -> Result<(), String> {
    match path {
        Some(path) => {
            std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
        }
        None => std::io::stdout()
            .write_all(text.as_bytes())
            .map_err(|e| format!("writing standard output: {e}")),
    }
}

/// An engine error as a message: the code and fixed text, plus, for a
/// broken lock, the facts (they are the user's own, on their terminal).
fn explain(error: &GhostwritinError) -> String {
    let mut message = format!("{} ({})", error, error.code());
    if let GhostwritinError::MeaningChanged { locks } = error {
        for lock in locks {
            let _ = write!(message, "\n  {:?}: {}", lock.kind, lock.text);
        }
    }
    message
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Rewrite {
            input,
            output,
            voice,
            strength,
            style,
            diff,
            json,
        } => {
            let text = read(&input)?;
            let style = style
                .map(|path| read(&path).and_then(|s| StyleSummary::new(s).map_err(|e| explain(&e))))
                .transpose()?;
            let model = model_from_env().ok_or(NO_MODEL)?;
            let request = RewriteRequest {
                text,
                voice,
                strength,
            };
            let response = pollster::block_on(Engine::new(model).rewrite(&request, style.as_ref()))
                .map_err(|e| explain(&e))?;
            if json {
                let body = serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?;
                write(output.as_deref(), &(body + "\n"))?;
            } else {
                write(output.as_deref(), &response.rewrite)?;
            }
            if diff {
                for segment in &response.diff {
                    let mark = match segment.op {
                        DiffOp::Removed => "[-",
                        DiffOp::Added => "{+",
                        DiffOp::Locked => "[=",
                        DiffOp::Same => "",
                    };
                    let close = match segment.op {
                        DiffOp::Removed => "-]",
                        DiffOp::Added => "+}",
                        DiffOp::Locked => "=]",
                        DiffOp::Same => "",
                    };
                    eprint!("{mark}{}{close}", segment.text);
                }
                eprintln!();
            }
            eprintln!(
                "ghostwritin: {} locked fact(s) kept; human score: not available in this build",
                response.locks.len()
            );
            Ok(())
        }
        Command::Voice {
            command: VoiceCommand::Learn { samples, output },
        } => {
            let texts = samples
                .iter()
                .map(|p| read(p))
                .collect::<Result<Vec<_>, _>>()?;
            let model = model_from_env().ok_or(NO_MODEL)?;
            let summary = pollster::block_on(VoiceBuilder::new(model).summarise(&texts))
                .map_err(|e| explain(&e))?;
            drop(texts);
            write(Some(&output), &format!("{}\n", summary.as_str()))?;
            eprintln!(
                "ghostwritin: style summary written to {} (the samples were not copied)",
                output.display()
            );
            Ok(())
        }
    }
}
