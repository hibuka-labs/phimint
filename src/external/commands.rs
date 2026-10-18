//! Parsing of the external-tool command surface (pure functions; product
//! copy and semantics live here, the TUI only forwards).
//!
//! Syntax (design "product surface" sketch + P3 delivery list):
//!
//! ```text
//! /connect                catalog overview
//! /connect <name>         connect one from the catalog (env readiness check → connect)
//! /connect <json…>        paste a config → wizard (secret extraction)
//! /tools                  panel: source / namespace / tier / switches
//! /tools off <key>        toggle one tool off (takes effect immediately)
//! /tools on <key>         toggle it back on
//! /tools remove <key>    delete a global script tool (key as the panel shows; ledger script.{name} cleaned in sync)
//! /plugin                 installed plugins overview (bare command = this namespace's view)
//! /plugin list            same as above
//! /plugin add <src>       install a plugin (persist after full human review)
//! /plugin remove <name>   uninstall a plugin (cleaned up in all three places)
//! ```

/// One external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCommand {
    /// `/connect` with no args: catalog overview + paste hint.
    ConnectHelp,
    /// `/connect <name>`: catalog hit.
    ConnectCatalog { name: String },
    /// `/connect <json…>`: paste → wizard.
    ConnectPaste { text: String },
    /// `/tools`: the panel.
    ToolsList,
    /// `/tools off <key>` / `/tools on <key>`: per-tool toggle (takes effect
    /// immediately).
    ToolToggle { key: String, on: bool },
    /// `/tools remove <key>`: delete a global script tool (+ledger cleanup).
    ToolRemove { name: String },
    /// Bare `/plugin` or `/plugin list`: installed plugins overview.
    PluginList,
    /// `/plugin add <src>`。
    PluginAdd { source: String },
    /// `/plugin remove <name>`。
    PluginRemove { name: String },
}

/// Parse one user input. Not an external command → `None` (goes through a
/// normal turn).
pub fn parse_command(input: &str) -> Option<ExternalCommand> {
    let text = input.trim();
    let rest = text.strip_prefix('/')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let head = parts.next()?;
    let tail = parts.next().unwrap_or("").trim();

    match head {
        "connect" => {
            if tail.is_empty() {
                Some(ExternalCommand::ConnectHelp)
            } else if tail.starts_with('{') {
                Some(ExternalCommand::ConnectPaste {
                    text: tail.to_string(),
                })
            } else {
                Some(ExternalCommand::ConnectCatalog {
                    name: tail.to_string(),
                })
            }
        }
        "tools" => {
            if tail.is_empty() {
                return Some(ExternalCommand::ToolsList);
            }
            let mut args = tail.splitn(2, char::is_whitespace);
            let sub = args.next()?;
            let arg = args.next().unwrap_or("").trim();
            match sub {
                "on" if !arg.is_empty() => Some(ExternalCommand::ToolToggle {
                    key: arg.to_string(),
                    on: true,
                }),
                "off" if !arg.is_empty() => Some(ExternalCommand::ToolToggle {
                    key: arg.to_string(),
                    on: false,
                }),
                "remove" if !arg.is_empty() => Some(ExternalCommand::ToolRemove {
                    name: arg.to_string(),
                }),
                _ => Some(ExternalCommand::ToolsList),
            }
        }
        "plugin" => {
            // Bare `/plugin` = this namespace's view, consistent with
            // `/connect` and `/tools`: bare command shows state, subcommands
            // add/remove.
            if tail.is_empty() {
                return Some(ExternalCommand::PluginList);
            }
            let mut args = tail.splitn(2, char::is_whitespace);
            let sub = args.next()?;
            let arg = args.next().unwrap_or("").trim();
            match sub {
                "list" if arg.is_empty() => Some(ExternalCommand::PluginList),
                "add" if !arg.is_empty() => Some(ExternalCommand::PluginAdd {
                    source: arg.to_string(),
                }),
                "remove" if !arg.is_empty() => Some(ExternalCommand::PluginRemove {
                    name: arg.to_string(),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Per-namespace usage hints (the decent error message on parse failure).
/// **Whichever namespace was hit, that is the one explained** — no more
/// cramming three commands into one line.
const USAGE_CONNECT: &str = "connect commands:
  /connect                 catalog overview
  /connect <name>          connect a catalog entry
  /connect <json...>       paste a config to open the wizard";
const USAGE_TOOLS: &str = "tools commands:
  /tools                   panel: source / namespace / tier / switches
  /tools on|off <key>      toggle one tool (takes effect immediately)
  /tools remove <key>      delete a global script tool (key as the panel shows)";
const USAGE_PLUGIN: &str = "plugin commands:
  /plugin list              installed plugins with their tools and tiers
  /plugin add <path|url>   install a plugin — review its full details, approve before it lands
  /plugin remove <name>    uninstall a plugin — the name /plugin list shows

after add you'll see the result confirm it: ✓ <name> installed · N tools
  (plugin.<name>.*), each tool key + what it does; then just ask me to use
  them by name in chat · /plugin list to review · /tools on|off <key> to
  toggle · /plugin remove to uninstall";

/// Lands in an external-command namespace but the syntax is incomplete
/// (`/plugin frobnicate x`) → that namespace's usage hint, **not sent to the
/// model as a turn** ([tests](parse_command) pin that the parse side returns
/// None for such inputs). Genuine free input returns `None` and goes through
/// a turn as usual.
pub fn usage_for(input: &str) -> Option<&'static str> {
    let rest = input.trim().strip_prefix('/')?;
    let head = rest.split_whitespace().next()?;
    match head {
        "connect" => Some(USAGE_CONNECT),
        "tools" => Some(USAGE_TOOLS),
        "plugin" => Some(USAGE_PLUGIN),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_forms() {
        assert_eq!(
            parse_command("/connect"),
            Some(ExternalCommand::ConnectHelp)
        );
        assert_eq!(
            parse_command("/connect notion"),
            Some(ExternalCommand::ConnectCatalog {
                name: "notion".into()
            })
        );
        assert_eq!(
            parse_command(r#"  /connect {"mcpServers": {"a": {"command": "x"}}} "#),
            Some(ExternalCommand::ConnectPaste {
                text: r#"{"mcpServers": {"a": {"command": "x"}}}"#.into()
            })
        );
    }

    #[test]
    fn tools_forms() {
        assert_eq!(parse_command("/tools"), Some(ExternalCommand::ToolsList));
        assert_eq!(
            parse_command("/tools off mcp.figma.get_file"),
            Some(ExternalCommand::ToolToggle {
                key: "mcp.figma.get_file".into(),
                on: false
            })
        );
        assert_eq!(
            parse_command("/tools on script.deploy"),
            Some(ExternalCommand::ToolToggle {
                key: "script.deploy".into(),
                on: true
            })
        );
        assert_eq!(
            parse_command("/tools remove deploy"),
            Some(ExternalCommand::ToolRemove {
                name: "deploy".into()
            })
        );
    }

    #[test]
    fn plugin_forms() {
        // Bare `/plugin` = list view (consistent with bare /connect and
        // /tools).
        assert_eq!(parse_command("/plugin"), Some(ExternalCommand::PluginList));
        assert_eq!(
            parse_command("/plugin list"),
            Some(ExternalCommand::PluginList)
        );
        assert_eq!(
            parse_command("/plugin add ~/plugins/my-tools"),
            Some(ExternalCommand::PluginAdd {
                source: "~/plugins/my-tools".into()
            })
        );
        assert_eq!(
            parse_command("/plugin remove my-tools"),
            Some(ExternalCommand::PluginRemove {
                name: "my-tools".into()
            })
        );
        // Unrecognized subcommand → not a command (not sent out as a turn,
        // and not silently dropped either).
        assert_eq!(parse_command("/plugin list extra junk"), None);
        assert_eq!(parse_command("/plugin frobnicate x"), None);
    }

    #[test]
    fn non_commands_pass_through() {
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("/resume"), None);
        assert_eq!(parse_command("/skill foo"), None);
        // Other people's namespaces must not eat our usage hints.
        assert_eq!(usage_for("hello"), None);
        assert_eq!(usage_for("/resume"), None);
    }

    #[test]
    fn usage_for_covers_namespace_misses() {
        // Whichever namespace was hit, show its usage (and multi-line in
        // columns, not one big hodgepodge line).
        // Note: valid input (incl. bare /plugin, /connect, /tools) already
        // matches a command on the parse side; usage_for is only consulted
        // when parse returns None (at the run.rs forward site); here the
        // three namespaces are pinned as a pure function.
        assert_eq!(usage_for("/plugin"), Some(USAGE_PLUGIN));
        assert_eq!(usage_for("/plugin frobnicate x"), Some(USAGE_PLUGIN));
        assert_eq!(usage_for("  /tools extra junk  "), Some(USAGE_TOOLS));
        assert_eq!(usage_for("/connect"), Some(USAGE_CONNECT));
        assert!(USAGE_PLUGIN.contains('\n'), "usage is multi-line");
        assert!(USAGE_PLUGIN.contains("/plugin add"));
        // The list closes the loop inside this namespace (no need to jump to
        // /tools just to see it).
        assert!(USAGE_PLUGIN.contains("/plugin list"), "list subcommand");
        // Closed-loop preview: the hint spells out "what you'll see after
        // install, and how to use and manage it afterwards".
        assert!(USAGE_PLUGIN.contains("✓ <name> installed"), "closed loop");
        assert!(USAGE_PLUGIN.contains("/tools"), "manage hint");
    }
}
