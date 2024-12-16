use clap::Parser;
use notify::{EventKind, RecursiveMode, Watcher};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Directory to watch for changes
    #[arg(short, long)]
    directory: PathBuf,

    /// Command to execute when changes are detected
    #[arg(short, long)]
    command: String,

    /// File extensions to watch (comma-separated, e.g., "rs,toml,json")
    #[arg(short, long, value_delimiter = ',')]
    extensions: Vec<String>,
}

fn is_relevant_event(event_kind: &EventKind) -> bool {
    use notify::event::*;
    matches!(
        event_kind,
        EventKind::Create(CreateKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Remove(RemoveKind::File)
    )
}

fn has_matching_extension(path: &std::path::Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| extensions.iter().any(|e| e == ext))
        .unwrap_or(false)
}

fn process_compiler_output(reader: BufReader<impl std::io::Read>, is_stderr: bool) {
    for line in reader.lines().filter_map(|line| line.ok()) {
        // Detect compiler errors and warnings
        if is_stderr || line.contains("error:") || line.contains("warning:") {
            eprintln!("\x1b[31m{}\x1b[0m", line); // Print in red
        } else {
            println!("{}", line);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let (tx, rx) = channel();

    let mut watcher = notify::recommended_watcher(move |res| {
        if let Ok(event) = res {
            tx.send(event).unwrap();
        }
    })?;

    watcher.watch(&cli.directory, RecursiveMode::Recursive)?;

    println!("Watching directory: {:?}", cli.directory);
    println!("Filtering for extensions: {:?}", cli.extensions);
    println!("Will execute command: {}", cli.command);
    println!("Waiting for file changes...");

    let command_parts: Vec<&str> = cli.command.split_whitespace().collect();
    if command_parts.is_empty() {
        return Err("Empty command provided".into());
    }

    let program = command_parts[0];
    let args = &command_parts[1..];

    let mut last_run = Instant::now() - Duration::from_secs(10);
    let debounce_duration = Duration::from_millis(500);

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => {
                if !is_relevant_event(&event.kind) {
                    continue;
                }

                let matching_path = event
                    .paths
                    .iter()
                    .any(|path| has_matching_extension(path, &cli.extensions));

                if !matching_path {
                    continue;
                }

                if last_run.elapsed() < debounce_duration {
                    continue;
                }

                last_run = Instant::now();
                println!("\nFile change detected! Event: {:?}", event.kind);
                println!("Changed files: {:?}", event.paths);
                println!("Executing command...\n");

                let mut child = Command::new(program)
                    .args(args)
                    .current_dir(&cli.directory)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;

                let stdout = child.stdout.take().expect("Failed to capture stdout");
                let stderr = child.stderr.take().expect("Failed to capture stderr");

                // Process stdout and stderr with proper error highlighting
                let stdout_thread = thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    process_compiler_output(reader, false);
                });

                let stderr_thread = thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    process_compiler_output(reader, true);
                });

                // Wait for output threads to complete
                stdout_thread.join().unwrap();
                stderr_thread.join().unwrap();

                // Wait for the command to complete
                match child.wait() {
                    Ok(status) => {
                        if !status.success() {
                            eprintln!("\n\x1b[31mCommand failed with status: {}\x1b[0m", status);
                            if let Some(code) = status.code() {
                                eprintln!("\x1b[31mExit code: {}\x1b[0m", code);
                            }
                        } else {
                            println!("\n\x1b[32mCommand completed successfully\x1b[0m");
                        }
                    }
                    Err(e) => eprintln!("\n\x1b[31mError waiting for command: {}\x1b[0m", e),
                }

                println!("\nWaiting for file changes...");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => {
                eprintln!("\x1b[31mWatch error: {:?}\x1b[0m", e);
                break;
            }
        }
    }

    Ok(())
}
