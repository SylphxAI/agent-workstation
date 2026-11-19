//! Version-synced progressive-disclosure usage instructions for agents.
//!
//! `agent-workstation skills` emits a small topic index stamped with the exact
//! crate version (`CARGO_PKG_VERSION`). Agents load only the section they need
//! (`core`, `desktop`, or `takeover`), keeping context compact and in lockstep
//! with the compiled CLI parser.

use clap::ValueEnum;

/// Crate version stamped into every rendered skill document.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Progressive-disclosure skill topics exposed by `agent-workstation skills`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkillTopic {
    /// Browser session lifecycle, snapshots, refs, fills, secrets, eval and close.
    Core,
    /// Native desktop window tree, focus, coordinates/ref clicks, typing, keys and scroll.
    Desktop,
    /// Human-in-the-loop noVNC takeover status, start, hand-back and atomic close.
    Takeover,
}

impl SkillTopic {
    /// Canonical lowercase CLI slug for this topic.
    pub const fn slug(self) -> &'static str {
        match self {
            SkillTopic::Core => "core",
            SkillTopic::Desktop => "desktop",
            SkillTopic::Takeover => "takeover",
        }
    }

    /// One-line summary shown in the progressive-disclosure index.
    pub const fn summary(self) -> &'static str {
        match self {
            SkillTopic::Core => {
                "Browser workflow: open, snapshot (@eN refs), click, fill, fill-secret, eval, screenshot, close"
            }
            SkillTopic::Desktop => {
                "Desktop workflow: desktop snapshot, windows, focus, click, type, key, scroll, screenshot"
            }
            SkillTopic::Takeover => {
                "Human handoff: takeover status/start/end, noVNC URL, human-in-control and atomic close"
            }
        }
    }
}

/// All available skill topics in deterministic display order.
pub const ALL_TOPICS: &[SkillTopic] =
    &[SkillTopic::Core, SkillTopic::Desktop, SkillTopic::Takeover];

/// Render either the compact topic index (`topic == None` or `list == true`)
/// or the requested topic section, stamped with [`VERSION`].
pub fn render(topic: Option<SkillTopic>, list: bool) -> String {
    if list {
        return render_index();
    }
    match topic {
        None => render_index(),
        Some(t) => render_topic(t),
    }
}

/// Render the compact progressive-disclosure index stamped with [`VERSION`].
pub fn render_index() -> String {
    let mut out = format!(
        "# agent-workstation skills (v{VERSION})\n\n\
         Load only the section needed for your current task:\n\n"
    );
    for topic in ALL_TOPICS {
        out.push_str(&format!(
            "- `{}`: {}\n  `agent-workstation skills {}`\n",
            topic.slug(),
            topic.summary(),
            topic.slug()
        ));
    }
    out.push_str(
        "\nGlobal flags: `--endpoint <url>` (`AGENT_WORKSTATION_URL`), \
         `--token <token>` (`AGENT_WORKSTATION_TOKEN`), `--session <id>` \
         (`AGENT_WORKSTATION_SESSION`), `--principal <name>`, `--json`.\n",
    );
    out
}

/// Render a single topic section stamped with [`VERSION`].
pub fn render_topic(topic: SkillTopic) -> String {
    match topic {
        SkillTopic::Core => format!(
            "# agent-workstation core (v{VERSION})\n\n\
             ## Workflow\n\
             1. Open a URL (creates or reuses the active session):\n\
                `agent-workstation open https://example.com`\n\
             2. Capture an accessibility snapshot with `@eN` refs:\n\
                `agent-workstation snapshot`\n\
             3. Interact using refs (`@e1` or snapshot-qualified `@s1:e1`) or CSS selectors:\n\
                `agent-workstation click @e1`\n\
                `agent-workstation fill @e2 \"user@example.com\"`\n\
                `agent-workstation fill-secret @e3 vault/account/password --expect-origin https://example.com`\n\
             4. Inspect page state or capture PNGs:\n\
                `agent-workstation eval \"document.title\"`\n\
                `agent-workstation screenshot ./page.png --full-page`\n\
             5. Atomically close the session when done (refuses if a human took over):\n\
                `agent-workstation close`\n\n\
             ## Rules\n\
             - Refs invalidate on navigation or newer snapshots (`stale-ref`); take a fresh `snapshot` before retrying.\n\
             - `fill-secret` resolves secrets daemon-side via `SecretResolver` and never echoes secret values to stdout or logs. Pass `--expect-origin` to fail closed on cross-origin frames.\n\
             - Pass `--new` to `open` to start a separate session, or `--json` on any command for structured output.\n\
             - Related sections: `agent-workstation skills desktop`, `agent-workstation skills takeover`.\n"
        ),
        SkillTopic::Desktop => format!(
            "# agent-workstation desktop (v{VERSION})\n\n\
             ## Commands\n\
             - Snapshot desktop windows with `@eN` / `@dN:eN` refs:\n\
               `agent-workstation desktop snapshot`\n\
             - List mapped windows or focus a window by id or ref:\n\
               `agent-workstation desktop windows`\n\
               `agent-workstation desktop focus @e1`\n\
             - Click by pixel coordinates (`<x> <y>`) or desktop ref (`@eN`):\n\
               `agent-workstation desktop click 640 400 --button left`\n\
               `agent-workstation desktop click @e1`\n\
             - Send keyboard chords, Unicode text, wheel scroll, or capture the full display:\n\
               `agent-workstation desktop key ctrl+l`\n\
               `agent-workstation desktop key Return --hold-ms 200`\n\
               `agent-workstation desktop type \"https://example.com\"`\n\
               `agent-workstation desktop scroll 640 400 --dx 0 --dy -3`\n\
               `agent-workstation desktop screenshot ./desktop.png`\n\n\
             ## Rules\n\
             - Desktop operations act on the same Wayland display as browser operations (browser chrome, file dialogs, cross-origin iframes).\n\
             - Key chords acknowledge compositor delivery, not app focus change; re-check state with `desktop snapshot` or `snapshot` before typing.\n\
             - Taking a new `desktop snapshot` invalidates qualified `@<prev>:eN` refs with `stale-ref`.\n"
        ),
        SkillTopic::Takeover => format!(
            "# agent-workstation takeover (v{VERSION})\n\n\
             ## Commands\n\
             - Inspect holder (`agent` or `human`), idle timer and noVNC URL:\n\
               `agent-workstation takeover`\n\
               `agent-workstation takeover status`\n\
             - Hand control to a person and obtain the authenticated noVNC URL:\n\
               `agent-workstation takeover start --label operator`\n\
             - End human control and return the session to the agent:\n\
               `agent-workstation takeover end --note \"sign-in complete\"`\n\
             - Close safely after hand-back (refuses while a human holds control):\n\
               `agent-workstation close`\n\
               `agent-workstation close --force`\n\n\
             ## Rules\n\
             - While a person holds control, agent operations return `human-in-control` (HTTP 409) until they hand back or the idle timer expires.\n\
             - Prefer `agent-workstation close` (atomic agent-only close) over `close --force` so an active human session is never cut off.\n\
             - Treat the noVNC URL as a bearer credential; share it only with the intended operator.\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command, DesktopCommand, TakeoverCommand};

    /// Extract backticked `agent-workstation ...` invocations from rendered skill text.
    fn extract_cli_invocations(doc: &str) -> Vec<Vec<String>> {
        let mut invocations = Vec::new();
        for segment in doc.split('`').skip(1).step_by(2) {
            let trimmed = segment.trim();
            if trimmed.starts_with("agent-workstation ") || trimmed == "agent-workstation" {
                invocations.push(split_shell_tokens(trimmed));
            }
        }
        invocations
    }

    fn split_shell_tokens(input: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        for ch in input.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                c if c.is_whitespace() && !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    #[test]
    fn every_rendered_skill_is_version_stamped_and_parses_with_clap() {
        let mut docs = vec![("index", render(None, false)), ("list", render(None, true))];
        for &topic in ALL_TOPICS {
            docs.push((topic.slug(), render(Some(topic), false)));
        }

        let mut seen_commands = BTreeSet::new();
        let mut total_invocations = 0usize;

        for (label, doc) in &docs {
            assert!(
                doc.contains(&format!("v{VERSION}")),
                "skill doc {label} must stamp exact CARGO_PKG_VERSION ({VERSION})"
            );
            let invocations = extract_cli_invocations(doc);
            assert!(
                !invocations.is_empty(),
                "skill doc {label} must contain verifiable CLI commands"
            );
            for argv in invocations {
                total_invocations += 1;
                let parsed = Cli::try_parse_from(&argv).unwrap_or_else(|err| {
                    panic!("skill doc {label} command {argv:?} failed clap parse: {err}")
                });
                match parsed.command {
                    Some(Command::Open { .. }) => {
                        seen_commands.insert("open");
                    }
                    Some(Command::Snapshot) => {
                        seen_commands.insert("snapshot");
                    }
                    Some(Command::Click { .. }) => {
                        seen_commands.insert("click");
                    }
                    Some(Command::Fill { .. }) => {
                        seen_commands.insert("fill");
                    }
                    Some(Command::FillSecret { .. }) => {
                        seen_commands.insert("fill-secret");
                    }
                    Some(Command::Eval { .. }) => {
                        seen_commands.insert("eval");
                    }
                    Some(Command::Screenshot { .. }) => {
                        seen_commands.insert("screenshot");
                    }
                    Some(Command::Close { .. }) => {
                        seen_commands.insert("close");
                    }
                    Some(Command::Skills { .. }) => {
                        seen_commands.insert("skills");
                    }
                    Some(Command::Desktop { action }) => {
                        seen_commands.insert("desktop");
                        match action {
                            DesktopCommand::Snapshot => {
                                seen_commands.insert("desktop snapshot");
                            }
                            DesktopCommand::Click { .. } => {
                                seen_commands.insert("desktop click");
                            }
                            DesktopCommand::Type { .. } => {
                                seen_commands.insert("desktop type");
                            }
                            DesktopCommand::Key { .. } => {
                                seen_commands.insert("desktop key");
                            }
                            DesktopCommand::Scroll { .. } => {
                                seen_commands.insert("desktop scroll");
                            }
                            DesktopCommand::Windows => {
                                seen_commands.insert("desktop windows");
                            }
                            DesktopCommand::Focus { .. } => {
                                seen_commands.insert("desktop focus");
                            }
                            DesktopCommand::Screenshot { .. } => {
                                seen_commands.insert("desktop screenshot");
                            }
                        }
                    }
                    Some(Command::Takeover { action }) => {
                        seen_commands.insert("takeover");
                        match action {
                            None => {
                                seen_commands.insert("takeover default");
                            }
                            Some(TakeoverCommand::Status) => {
                                seen_commands.insert("takeover status");
                            }
                            Some(TakeoverCommand::Start { .. }) => {
                                seen_commands.insert("takeover start");
                            }
                            Some(TakeoverCommand::End { .. }) => {
                                seen_commands.insert("takeover end");
                            }
                        }
                    }
                    Some(Command::Daemon(_)) | None => {}
                }
            }
        }

        assert!(total_invocations >= 20, "expected full CLI coverage");
        for required in [
            "open",
            "snapshot",
            "click",
            "fill",
            "fill-secret",
            "eval",
            "screenshot",
            "desktop",
            "desktop snapshot",
            "desktop click",
            "desktop type",
            "desktop key",
            "desktop scroll",
            "desktop windows",
            "desktop focus",
            "desktop screenshot",
            "takeover",
            "takeover default",
            "takeover status",
            "takeover start",
            "takeover end",
            "close",
            "skills",
        ] {
            assert!(
                seen_commands.contains(required),
                "missing skill documentation for subcommand `{required}`"
            );
        }
    }

    #[test]
    fn list_flag_overrides_topic_to_show_compact_index() {
        assert_eq!(render(Some(SkillTopic::Core), true), render_index());
        assert_eq!(render(None, false), render_index());
    }
}
