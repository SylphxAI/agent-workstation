//! Command-line interface schema for the `agent-workstation` binary.
//!
//! Supports both the standalone HTTP daemon mode (via top-level daemon flags for
//! backwards compatibility or the explicit `daemon` subcommand) and compact
//! client subcommands that drive a running daemon session, plus offline
//! version-synced `skills` documentation.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::desktop::Button;
use crate::error::{Error, Result};
use crate::skills::SkillTopic;

/// Default daemon listen address and client endpoint.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7600";
/// Default client endpoint URL.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:7600";

/// Top-level CLI for `agent-workstation`.
#[derive(Parser, Debug)]
#[command(
    name = "agent-workstation",
    version,
    about = "Headless Wayland browser and desktop automation daemon and CLI"
)]
pub struct Cli {
    /// Connection and output options shared across client subcommands.
    #[command(flatten)]
    pub client: ClientArgs,

    /// Legacy top-level daemon flags (used when no subcommand is supplied).
    #[command(flatten)]
    pub daemon: DaemonArgs,

    /// Subcommand to execute. Omit to run the HTTP daemon with top-level flags.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Global client options for talking to a running `agent-workstation` daemon.
#[derive(Args, Clone)]
pub struct ClientArgs {
    /// Daemon HTTP endpoint URL.
    #[arg(
        long,
        env = "AGENT_WORKSTATION_URL",
        default_value = DEFAULT_ENDPOINT,
        global = true
    )]
    pub endpoint: String,

    /// Bearer token for authenticating with the daemon (`16+` characters).
    #[arg(long, env = "AGENT_WORKSTATION_TOKEN", global = true)]
    pub token: Option<String>,

    /// Explicit session id to target (overrides saved session file).
    #[arg(long, env = "AGENT_WORKSTATION_SESSION", global = true)]
    pub session: Option<String>,

    /// Agent principal name used to lease and call the session.
    #[arg(
        long,
        env = "AGENT_WORKSTATION_PRINCIPAL",
        default_value = "cli",
        global = true
    )]
    pub principal: String,

    /// Local file tracking the active session id across compact CLI calls.
    #[arg(long, env = "AGENT_WORKSTATION_SESSION_FILE", global = true)]
    pub session_file: Option<PathBuf>,

    /// Emit raw structured JSON responses instead of compact human/agent text.
    #[arg(long, global = true, default_value_t = false)]
    pub json: bool,
}

impl std::fmt::Debug for ClientArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientArgs")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("session", &self.session)
            .field("principal", &self.principal)
            .field("session_file", &self.session_file)
            .field("json", &self.json)
            .finish()
    }
}

/// Options for running the standalone HTTP daemon.
#[derive(Args, Clone, Debug)]
pub struct DaemonArgs {
    /// Address and port to listen on.
    #[arg(long, default_value = DEFAULT_LISTEN)]
    pub listen: SocketAddr,

    /// Directory holding per-session runtime sockets and logs.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,

    /// Chromium-family browser binary path.
    #[arg(long)]
    pub browser: Option<PathBuf>,

    /// Optional flag passed as `<flag>=<seed>` when launching the browser.
    #[arg(long, allow_hyphen_values = true)]
    pub seed_flag: Option<String>,

    /// Extra browser command-line arguments (repeatable).
    #[arg(long = "browser-arg", action = ArgAction::Append, allow_hyphen_values = true)]
    pub browser_args: Vec<String>,

    /// External secret-reader command terminated by `--` (receives `<secret_ref>` as final arg).
    #[arg(
        long,
        num_args = 1..,
        value_terminator = "--",
        allow_hyphen_values = true
    )]
    pub secret_command: Option<Vec<String>>,
}

/// Top-level CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the standalone HTTP daemon explicitly.
    Daemon(DaemonArgs),

    /// Start or attach to a session and navigate to a URL.
    Open {
        /// Destination URL to open.
        url: String,
        /// Profile name used when starting a new session.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Persistent browser profile directory when starting a new session.
        #[arg(long, env = "AGENT_WORKSTATION_PROFILE_DIR")]
        profile_dir: Option<PathBuf>,
        /// Optional egress proxy URL for a new session.
        #[arg(long)]
        proxy: Option<String>,
        /// Always start a new session instead of reusing the attached session.
        #[arg(long, default_value_t = false)]
        new: bool,
    },

    /// Capture a structured accessibility/DOM snapshot with `@eN` refs.
    Snapshot,

    /// Click an element by `@eN` / `@<snap>:eN` ref or CSS selector.
    Click {
        /// Element ref (`@e1`, `@s1:e1`) or CSS selector.
        #[arg(value_name = "REF")]
        target: String,
    },

    /// Replace an input or textarea value by ref or CSS selector.
    Fill {
        /// Element ref (`@e1`, `@s1:e1`) or CSS selector.
        #[arg(value_name = "REF")]
        target: String,
        /// Text value to fill.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },

    /// Resolve a secret handle server-side via `SecretResolver` and insert it without echoing.
    #[command(name = "fill-secret")]
    FillSecret {
        /// Element ref (`@e1`, `@s1:e1`) or CSS selector.
        #[arg(value_name = "REF")]
        target: String,
        /// Caller-defined secret reference handle resolved by the daemon.
        #[arg(value_name = "SECRET_HANDLE")]
        secret_handle: String,
        /// Optional expected frame origin (`https://example.com`); refuses cross-origin fills.
        #[arg(long)]
        expect_origin: Option<String>,
    },

    /// Evaluate a JavaScript expression in the active page.
    Eval {
        /// JavaScript expression to evaluate.
        #[arg(allow_hyphen_values = true)]
        js: String,
    },

    /// Discover page-defined WebMCP tools.
    WebmcpList,

    /// Invoke a page-defined WebMCP tool with a JSON argument object.
    WebmcpCall {
        /// Opaque tool ref returned by webmcp-list.
        tool_ref: String,
        /// JSON argument object.
        #[arg(default_value = "{}")]
        arguments: String,
        /// The caller obtained human approval for this operation.
        #[arg(long)]
        confirmed: bool,
        /// Deadline in milliseconds (1–20000).
        #[arg(long, default_value_t = crate::WEBMCP_DEFAULT_TIMEOUT_MS)]
        timeout_ms: u64,
    },

    /// Capture a PNG screenshot of the active page (or full desktop with `--desktop`).
    Screenshot {
        /// Optional output file path for the PNG bytes.
        path: Option<PathBuf>,
        /// Capture the full scrollable page instead of only the viewport.
        #[arg(long, default_value_t = false)]
        full_page: bool,
        /// Capture the full Wayland desktop instead of only the browser page.
        #[arg(long, default_value_t = false)]
        desktop: bool,
    },

    /// Native desktop operations (windows, snapshot, pointer, keyboard, screenshots).
    Desktop {
        /// Target the local host OS desktop directly via the compile-time `PlatformAdapter`
        /// (AT-SPI2/Wayland on Linux, AXUIElement on macOS, UI Automation on Windows)
        /// instead of the remote daemon session.
        #[arg(long, default_value_t = false)]
        host: bool,
        /// Desktop action to perform.
        #[command(subcommand)]
        action: DesktopCommand,
    },

    /// Inspect, start, or end human noVNC takeover for the session.
    Takeover {
        /// Takeover action (defaults to `status` when omitted).
        #[command(subcommand)]
        action: Option<TakeoverCommand>,
    },

    /// Atomically close the active session (refuses if a human holds control unless `--force`).
    Close {
        /// Force unconditional close even while a person holds takeover control.
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Print version-synced progressive-disclosure usage instructions for agents.
    Skills {
        /// Skill section to print (`core`, `desktop`, `takeover`). Omit for the index.
        #[arg(value_enum)]
        topic: Option<SkillTopic>,
        /// Print the compact index of available skill sections.
        #[arg(long, default_value_t = false)]
        list: bool,
    },
}

/// Subcommands under `agent-workstation desktop`.
#[derive(Subcommand, Debug)]
pub enum DesktopCommand {
    /// Inspect capabilities of the native desktop adapter.
    Capabilities,

    /// Capture a structured accessibility/window snapshot with `@eN` / `@<snap>:eN` refs.
    Snapshot {
        /// Maximum accessibility tree depth.
        #[arg(long)]
        max_depth: Option<usize>,
        /// Maximum accessibility tree nodes.
        #[arg(long)]
        max_nodes: Option<usize>,
        /// Optional target process id filter.
        #[arg(long)]
        pid: Option<u32>,
        /// Capture a subtree at a current qualified ref (session mode).
        #[arg(long)]
        root: Option<String>,
        /// Skip this many immediate children, or top-level roots (session mode).
        #[arg(long)]
        offset: Option<usize>,
    },

    /// Click at desktop pixel coordinates (`<x> <y>`) or a desktop snapshot ref (`@e1`).
    Click {
        /// Either X pixel coordinate (paired with `y`), `"x,y"`, or a desktop ref (`@e1`).
        #[arg(allow_hyphen_values = true)]
        target: String,
        /// Y pixel coordinate when clicking by coordinates.
        #[arg(allow_hyphen_values = true)]
        y: Option<f64>,
        /// Pointer button (defaults to `left`).
        #[arg(long, value_enum)]
        button: Option<CliButton>,
        /// Optional modifier chord held around the click (e.g. `ctrl+shift`).
        #[arg(long)]
        modifiers: Option<String>,
    },

    /// Double-click a desktop snapshot ref (`@e1` or `@<snap>:e1`).
    #[command(name = "double-click")]
    DoubleClick {
        /// Desktop snapshot ref (`@e1` or `@<snap>:e1`).
        #[arg(value_name = "REF")]
        target: String,
    },

    /// Replace an accessible control's value directly by ref (`@e1` or `@<snap>:e1`).
    #[command(name = "set-value")]
    SetValue {
        /// Desktop snapshot ref (`@e1` or `@<snap>:e1`).
        #[arg(value_name = "REF")]
        target: String,
        /// Replacement text value.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },

    /// Type Unicode text into the currently focused desktop window.
    Type {
        /// Text to type.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },

    /// Press (or hold) a desktop key chord (e.g. `ctrl+l`, `Return`).
    Key {
        /// Key chord to press.
        chord: String,
        /// Hold the chord for this many milliseconds before releasing.
        #[arg(long)]
        hold_ms: Option<u64>,
    },

    /// Scroll at desktop coordinates by wheel notches.
    Scroll {
        /// X coordinate in desktop pixels.
        #[arg(allow_hyphen_values = true)]
        x: f64,
        /// Y coordinate in desktop pixels.
        #[arg(allow_hyphen_values = true)]
        y: f64,
        /// Horizontal notches (positive = right).
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        dx: i32,
        /// Vertical notches (positive = down).
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        dy: i32,
    },

    /// List mapped desktop windows with ids, titles, app ids, focus state and bounds.
    Windows,

    /// Focus a desktop window by numeric compositor id or `@eN` / `@<snap>:eN` ref.
    Focus {
        /// Numeric compositor window id or desktop snapshot ref (`@e1`).
        target: String,
    },

    /// Capture a full-desktop PNG screenshot.
    Screenshot {
        /// Optional output file path for the PNG bytes.
        path: Option<PathBuf>,
    },
}

/// Subcommands under `agent-workstation takeover`.
#[derive(Subcommand, Debug)]
pub enum TakeoverCommand {
    /// Show current control holder, idle timer and noVNC URL.
    Status,
    /// Hand control to a person and print the authenticated noVNC takeover URL.
    Start {
        /// Optional audit label naming the human operator.
        #[arg(long)]
        label: Option<String>,
    },
    /// End human takeover and return control to the agent.
    End {
        /// Optional hand-back note passed to the agent.
        #[arg(long, allow_hyphen_values = true)]
        note: Option<String>,
    },
}

/// Pointer button choices for CLI flags.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliButton {
    /// Primary pointer button.
    Left,
    /// Middle pointer button.
    Middle,
    /// Secondary pointer button.
    Right,
    /// Browser history back button.
    Back,
    /// Browser history forward button.
    Forward,
}

impl From<CliButton> for Button {
    fn from(b: CliButton) -> Self {
        match b {
            CliButton::Left => Button::Left,
            CliButton::Middle => Button::Middle,
            CliButton::Right => Button::Right,
            CliButton::Back => Button::Back,
            CliButton::Forward => Button::Forward,
        }
    }
}

/// Parsed target of `agent-workstation desktop click`.
#[derive(Clone, Debug, PartialEq)]
pub enum DesktopClickTarget {
    /// Click at absolute desktop output coordinates.
    Coords { x: f64, y: f64 },
    /// Click the centre of a desktop snapshot ref (`@e1` or `@d1:e1`).
    Ref(String),
}

/// Parse the positional arguments of `desktop click` into coordinates or a ref.
pub fn parse_desktop_click_target(target: &str, y: Option<f64>) -> Result<DesktopClickTarget> {
    if let Some(y) = y {
        let x: f64 = target
            .trim()
            .parse()
            .map_err(|_| Error::InvalidInput(format!("invalid x coordinate {target:?}")))?;
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidInput("coordinates must be finite".into()));
        }
        return Ok(DesktopClickTarget::Coords { x, y });
    }
    let trimmed = target.trim();
    if let Some((raw_x, raw_y)) = trimmed.split_once(',') {
        let x: f64 = raw_x
            .trim()
            .parse()
            .map_err(|_| Error::InvalidInput(format!("invalid x coordinate {raw_x:?}")))?;
        let y: f64 = raw_y
            .trim()
            .parse()
            .map_err(|_| Error::InvalidInput(format!("invalid y coordinate {raw_y:?}")))?;
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidInput("coordinates must be finite".into()));
        }
        return Ok(DesktopClickTarget::Coords { x, y });
    }
    if trimmed.starts_with('@')
        || (trimmed.len() > 1
            && trimmed.starts_with('e')
            && trimmed[1..].bytes().all(|b| b.is_ascii_digit()))
    {
        return Ok(DesktopClickTarget::Ref(trimmed.to_owned()));
    }
    Err(Error::InvalidInput(format!(
        "desktop click expects <x> <y> or a desktop ref like @e1 (got {target:?})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_daemon_flags_remain_backwards_compatible() {
        let cli = Cli::try_parse_from([
            "agent-workstation",
            "--listen",
            "127.0.0.1:0",
            "--state-dir",
            "/var/lib/ws",
            "--browser",
            "/usr/bin/chromium",
            "--seed-flag",
            "--fingerprint",
            "--browser-arg",
            "--no-sandbox",
            "--browser-arg",
            "--disable-gpu",
            "--secret-command",
            "my-reader",
            "--flag",
            "val",
            "--",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.daemon.listen.port(), 0);
        assert_eq!(
            cli.daemon.state_dir.as_deref(),
            Some(std::path::Path::new("/var/lib/ws"))
        );
        assert_eq!(
            cli.daemon.browser.as_deref(),
            Some(std::path::Path::new("/usr/bin/chromium"))
        );
        assert_eq!(cli.daemon.seed_flag.as_deref(), Some("--fingerprint"));
        assert_eq!(
            cli.daemon.browser_args,
            vec!["--no-sandbox", "--disable-gpu"]
        );
        assert_eq!(
            cli.daemon.secret_command,
            Some(vec!["my-reader".into(), "--flag".into(), "val".into()])
        );
    }

    #[test]
    fn secret_command_terminator_allows_subsequent_daemon_flags() {
        let cli = Cli::try_parse_from([
            "agent-workstation",
            "--secret-command",
            "reader",
            "-v",
            "--",
            "--state-dir",
            "/tmp/s",
            "--browser",
            "/usr/bin/chromium",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert_eq!(
            cli.daemon.secret_command,
            Some(vec!["reader".into(), "-v".into()])
        );
        assert_eq!(
            cli.daemon.browser.as_deref(),
            Some(std::path::Path::new("/usr/bin/chromium"))
        );
    }

    #[test]
    fn client_args_debug_redacts_token() {
        let cli = Cli::try_parse_from([
            "agent-workstation",
            "--token",
            "super-secret-bearer-token-value",
            "snapshot",
        ])
        .unwrap();
        let dbg = format!("{:?}", cli.client);
        assert!(!dbg.contains("super-secret-bearer-token-value"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn desktop_click_target_parses_coords_pairs_and_refs() {
        assert_eq!(
            parse_desktop_click_target("640", Some(400.0)).unwrap(),
            DesktopClickTarget::Coords { x: 640.0, y: 400.0 }
        );
        assert_eq!(
            parse_desktop_click_target("640,400", None).unwrap(),
            DesktopClickTarget::Coords { x: 640.0, y: 400.0 }
        );
        assert_eq!(
            parse_desktop_click_target("@e2", None).unwrap(),
            DesktopClickTarget::Ref("@e2".into())
        );
        assert_eq!(
            parse_desktop_click_target("@d1:e2", None).unwrap(),
            DesktopClickTarget::Ref("@d1:e2".into())
        );
        assert_eq!(
            parse_desktop_click_target("640", None).unwrap_err().code(),
            "invalid-input"
        );
    }

    #[test]
    fn negative_scroll_deltas_and_hyphen_values_parse_cleanly() {
        let cli = Cli::try_parse_from([
            "agent-workstation",
            "desktop",
            "scroll",
            "640",
            "400",
            "--dx",
            "-2",
            "--dy",
            "-5",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Desktop {
                action: DesktopCommand::Scroll { x, y, dx, dy },
                ..
            }) => {
                assert_eq!((x, y, dx, dy), (640.0, 400.0, -2, -5));
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        let fill = Cli::try_parse_from(["agent-workstation", "fill", "@e1", "-42"]).unwrap();
        assert!(matches!(
            fill.command,
            Some(Command::Fill { ref target, ref value }) if target == "@e1" && value == "-42"
        ));
    }
}
