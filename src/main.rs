use clap::Parser;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
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

// Keep last few events for smarter debouncing
struct EventBuffer {
    events: VecDeque<Instant>,
    window: Duration,
}

impl EventBuffer {
    fn new(window: Duration) -> Self {
        Self {
            events: VecDeque::new(),
            window,
        }
    }

    fn add_event(&mut self, now: Instant) {
        // Remove old events outside the window
        while let Some(time) = self.events.front() {
            if now.duration_since(*time) > self.window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        self.events.push_back(now);
    }

    fn should_trigger(&self, min_quiet_period: Duration) -> bool {
        if let Some(last_event) = self.events.back() {
            // If we've had a quiet period and have some events, trigger
            Instant::now().duration_since(*last_event) >= min_quiet_period
                && !self.events.is_empty()
        } else {
            false
        }
    }

    fn clear(&mut self) {
        self.events.clear();
    }
}

fn get_user_shell() -> (String, String) {
    if let Ok(shell) = std::env::var("SHELL") {
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sh")
            .to_string();

        let rc_command = match shell_name.as_str() {
            "zsh" => "source ~/.zshrc 2>/dev/null || true",
            "bash" => "source ~/.bashrc 2>/dev/null || source ~/.bash_profile 2>/dev/null || true",
            _ => "true",
        };

        (shell, rc_command.to_string())
    } else {
        ("/bin/sh".to_string(), "true".to_string())
    }
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

fn process_output(reader: BufReader<impl std::io::Read>, is_stderr: bool) {
    for line in reader.lines().filter_map(|line| line.ok()) {
        if is_stderr {
            eprintln!("\x1b[31m{}\x1b[0m", line);
        } else {
            println!("{}", line);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let (shell, rc_command) = get_user_shell();

    let (tx, rx) = channel();

    let mut watcher = notify::recommended_watcher(move |res| {
        if let Ok(event) = res {
            tx.send(event).unwrap();
        }
    })?;

    watcher.watch(&cli.directory, RecursiveMode::Recursive)?;

    println!("Watching directory: {:?}", cli.directory);
    println!("Filtering for extensions: {:?}", cli.extensions);
    println!("Using shell: {}", shell);
    println!("Will execute command: {}", cli.command);
    println!("Waiting for file changes...");

    let shell_command = if cfg!(target_os = "windows") {
        cli.command.clone()
    } else {
        format!("{rc_command}; {}", cli.command)
    };

    // Configure debouncing
    let mut event_buffer = EventBuffer::new(Duration::from_millis(1000));
    let quiet_period = Duration::from_millis(500);

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
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

                event_buffer.add_event(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {
                // Check if we should trigger based on the event buffer
                if event_buffer.should_trigger(quiet_period) {
                    println!("\nFile change detected!");
                    println!("Executing command...\n");

                    let mut child = if cfg!(target_os = "windows") {
                        Command::new("cmd")
                            .args(["/C", &shell_command])
                            .current_dir(&cli.directory)
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()?
                    } else {
                        Command::new(&shell)
                            .args(["-l", "-c", &shell_command])
                            .current_dir(&cli.directory)
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()?
                    };

                    let stdout = child.stdout.take().expect("Failed to capture stdout");
                    let stderr = child.stderr.take().expect("Failed to capture stderr");

                    let stdout_thread = thread::spawn(move || {
                        let reader = BufReader::new(stdout);
                        process_output(reader, false);
                    });

                    let stderr_thread = thread::spawn(move || {
                        let reader = BufReader::new(stderr);
                        process_output(reader, true);
                    });

                    stdout_thread.join().unwrap();
                    stderr_thread.join().unwrap();

                    match child.wait() {
                        Ok(status) => {
                            if !status.success() {
                                eprintln!(
                                    "\n\x1b[31mCommand failed with status: {}\x1b[0m",
                                    status
                                );
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
                    event_buffer.clear();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("\x1b[31mWatch error: channel disconnected\x1b[0m");
                break;
            }
        }
    }

    Ok(())
}
