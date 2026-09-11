//! `ah`: CLI one-shot mode and TUI.

mod app;
mod cli;
mod plugin_source;
#[cfg(feature = "remote")]
mod remote;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ah",
    version,
    about = "Minimal, fast agent harness (OpenRouter + wasm plugins)"
)]
pub struct Cli {
    /// Prompt for one-shot mode. Also read from stdin when piped.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Prompt words (same as --prompt).
    #[arg(trailing_var_arg = true)]
    pub words: Vec<String>,

    #[command(flatten)]
    pub overrides: Overrides,

    /// Emit JSONL events instead of text (one-shot mode).
    #[arg(long)]
    pub json: bool,

    /// Resume a session by id or name (default: latest).
    #[arg(short, long, value_name = "ID|NAME", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Force the TUI even when a prompt is given (prompt is submitted first).
    #[arg(long)]
    pub tui: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Settings overrides shared by every mode. Applied as the last layer.
#[derive(Args, Debug, Default, Clone)]
pub struct Overrides {
    /// Model id, e.g. anthropic/claude-sonnet-4.5
    #[arg(short, long)]
    pub model: Option<String>,

    /// Auto-approve every tool call (default).
    #[arg(long, conflicts_with = "ask")]
    pub yolo: bool,

    /// Ask before bash/write/edit tool calls.
    #[arg(long)]
    pub ask: bool,

    /// Do not load any plugins.
    #[arg(long)]
    pub no_plugins: bool,

    /// Extra plugin file or directory (repeatable).
    #[arg(long = "plugin", value_name = "PATH")]
    pub plugins: Vec<PathBuf>,

    /// Working directory for tools.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Extra text appended to the system prompt.
    #[arg(short, long, value_name = "TEXT")]
    pub system: Option<String>,

    #[arg(long)]
    pub max_tokens: Option<u32>,

    /// Override any setting: --set theme.accent=magenta --set layout.input_height=5
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub sets: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show auth status and store an OpenRouter API key (prompted, hidden).
    Login {
        /// Key to store without prompting. Prefer the prompt or piping it on
        /// stdin: this lands in your shell history.
        #[arg(long)]
        key: Option<String>,
    },
    /// Remove the stored API key.
    Logout,
    /// List models available on OpenRouter.
    Models {
        /// Only models that support tool calling.
        #[arg(long)]
        tools: bool,
        /// Only models producing this output: text, image, video, speech,
        /// transcription, embeddings, rerank.
        #[arg(long, value_name = "KIND")]
        modality: Option<String>,
        /// Fuzzy filter on id and name.
        filter: Option<String>,
        /// Re-fetch even if the local cache is fresh.
        #[arg(long)]
        refresh: bool,
    },
    /// Manage wasm plugins.
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },
    /// Show or initialise configuration.
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// List stored sessions.
    Sessions,
    /// Pair this machine with a phone, and see what the link is doing.
    #[cfg(feature = "remote")]
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Print the built-in documentation: `ah docs` lists topics, `ah docs plugins` prints one.
    Docs {
        /// Topic name (config, keys, commands, instructions, skills, plugins, sessions).
        topic: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// List discovered plugins and their load status.
    List,
    /// Copy a .wasm file into the user plugin directory.
    Add { path: PathBuf },
    /// Remove a plugin from the user plugin directory by name.
    Rm { name: String },
    /// Build a plugin crate for wasm32 and install it.
    Build {
        /// Path to the plugin crate (default: current directory).
        dir: Option<PathBuf>,
        /// Only build; do not copy into the plugin directory.
        #[arg(long)]
        no_install: bool,
    },
    /// Fetch a plugin from a git repository, build it if needed, and install it.
    Install {
        /// Git URL or local path, `owner/repo` on GitHub, or a GitHub/GitLab
        /// `.../tree/<ref>/<path>` link pointing at the plugin directory.
        source: String,
        /// Directory inside the repository that holds the plugin.
        subdir: Option<String>,
        /// Branch, tag or commit to check out.
        #[arg(long = "ref", value_name = "REF")]
        git_ref: Option<String>,
    },
    /// Reinstall plugins that came from git from their recorded sources.
    Update {
        /// Only this plugin (default: every one with a recorded source).
        name: Option<String>,
    },
}

#[cfg(feature = "remote")]
#[derive(Subcommand, Debug)]
pub enum RemoteCmd {
    /// Make a new pairing and show the code to scan.
    Pair {
        /// The relay to pair against. Remembered, so it is needed once.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
    /// Publish this machine's sessions with no window open.
    Serve {
        /// Carry on in the background and give the terminal back.
        #[arg(long)]
        detach: bool,
    },
    /// Whether this machine is paired, and to what.
    Status,
    /// Forget the pairing, so no phone holding it can reach this machine.
    Forget,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the fully merged settings as TOML (default).
    Show {
        /// Also list the layers that contributed.
        #[arg(long)]
        origins: bool,
    },
    /// Print config file locations.
    Path,
    /// Write a commented default config.toml to the user config dir.
    Init {
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    // skip clap for --version
    let mut args = std::env::args_os().skip(1);
    if let (Some(a), None) = (args.next(), args.next())
        && (a == "--version" || a == "-V")
    {
        println!("ah {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ah: {e}");
            1
        }
    };
    // Agents and background jobs are children of this process; none outlives
    // it. Agents stop first, or one of them starts a command after the sweep.
    let agents = ah_core::agents::table().running();
    if agents > 0 {
        eprintln!("ah: stopping {agents} agent(s)");
        ah_core::agents::table().shutdown(std::time::Duration::from_secs(2));
    }
    let running = ah_core::jobs::table().running();
    if running > 0 {
        eprintln!("ah: stopping {running} background job(s)");
        ah_core::jobs::table().shutdown(std::time::Duration::from_millis(500));
    }
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(cmd) = cli.command {
        return cli::subcommand(cmd, &cli.overrides);
    }
    let mut prompt = cli.prompt.clone();
    if prompt.is_none() && !cli.words.is_empty() {
        prompt = Some(cli.words.join(" "));
    }
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    if prompt.is_none() && !stdin_is_tty {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        if !s.trim().is_empty() {
            prompt = Some(s);
        }
    }
    let want_tui = cli.tui || (prompt.is_none() && stdin_is_tty && stdout_is_tty);
    if want_tui {
        tui::run(&cli.overrides, cli.resume.as_deref(), prompt)
    } else {
        match prompt {
            Some(p) => cli::one_shot(&cli.overrides, cli.resume.as_deref(), &p, cli.json),
            None => Err("no prompt given and no terminal for the TUI; try `ah -p \"...\"`".into()),
        }
    }
}
