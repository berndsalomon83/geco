//! Ask a language model which candidate tool a request actually means.
//!
//! WHY: geco's lexical ranking (BM25F + synonyms) puts the right tool first
//! only ~13% of the time when the request shares no vocabulary with it — a
//! user asking to "grab the contents of a webpage" does not type the words
//! `web_url_read` uses. Dictionaries, parameter tuning and embedding rerank
//! were all measured against that wall and none crossed it
//! (`docs/search-ranking.md`).
//!
//! A real model does. Measured on 210 vocabulary-banned queries over 5,216
//! real tools: handing BM25F's top-50 to a model and asking which one the user
//! meant scored **40% rank-1 versus lexical's 13.3%**, and — the number that
//! matters — it chose correctly **87% of the time when the answer was in the
//! list at all**. The semantic judgement is essentially solved; the only
//! remaining lever is how wide the candidate net is.
//!
//! HARNESS-AGNOSTIC BY DESIGN. There is no single way to reach "the model the
//! user is already paying for", so this supports three, tried in preference
//! order:
//!
//!   1. MCP sampling — protocol-native, zero config, uses the client's own
//!      model. Only possible when the client advertises the capability, which
//!      today most do not.
//!   2. Any command — `opencode run …`, `claude -p`, `codex exec`, or a
//!      two-line wrapper around an OpenAI-compatible endpoint (LM Studio,
//!      Ollama, vLLM, a hosted API).
//!
//! There is deliberately no built-in HTTP client. Adding one would drag a TLS
//! stack into a binary whose whole pitch is "no runtime dependencies", to do
//! a job `curl` in a three-line script already does. The user brings the
//! transport; geco stays a single static binary.
//!
//! EVERY failure — unsupported, unreachable, malformed reply, timeout — falls
//! back to the lexical order. Enabling a picker can add latency; it cannot
//! break search.

use std::time::Duration;

use crate::config::PickerConfig;

/// One thing the model may choose between.
pub struct Candidate<'a> {
    pub server: &'a str,
    pub tool: &'a str,
    pub description: &'a str,
}

/// Which transport reaches a model.
#[derive(Debug, Clone, PartialEq)]
pub enum Transport {
    /// No picker: lexical ranking only (the default).
    Off,
    /// Ask the connected MCP client via `sampling/createMessage`.
    Sampling,
    /// Spawn a command and read its stdout.
    Command(Vec<String>),
}

pub struct Picker {
    transport: Transport,
    pub candidates: usize,
    timeout: Duration,
}

impl Picker {
    /// Resolve the configured mode against what is actually available.
    /// `client_supports_sampling` comes from the MCP initialize handshake.
    pub fn from_config(cfg: &PickerConfig, client_supports_sampling: bool) -> Self {
        let has_cmd = !cfg.command.is_empty();
        let transport = match cfg.mode.as_str() {
            "sampling" if client_supports_sampling => Transport::Sampling,
            "command" if has_cmd => Transport::Command(cfg.command.clone()),
            // `auto` takes the best thing actually available, cheapest first:
            // the client's own model costs the user nothing extra.
            "auto" => {
                if client_supports_sampling {
                    Transport::Sampling
                } else if has_cmd {
                    Transport::Command(cfg.command.clone())
                } else {
                    Transport::Off
                }
            }
            // Explicitly-requested-but-unavailable lands here too: a config
            // asking for `sampling` against a client that cannot do it must
            // degrade to lexical, not error the search.
            _ => Transport::Off,
        };
        Self {
            transport,
            candidates: cfg.candidates.clamp(1, 400),
            timeout: Duration::from_secs(cfg.timeout_secs.clamp(1, 300)),
        }
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    pub fn is_enabled(&self) -> bool {
        self.transport != Transport::Off
    }

    /// The prompt every transport sends. Kept in one place so what was
    /// measured is what ships: state the request, list the candidates, demand
    /// a bare JSON object naming one of them.
    pub fn prompt(query: &str, cands: &[Candidate<'_>]) -> String {
        let list = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "{}. {}::{} — {}",
                    i + 1,
                    c.server,
                    c.tool,
                    c.description.chars().take(160).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "A user asked: \"{query}\"\n\n\
             Pick the ONE tool below that best fulfils that request.\n\n\
             {list}\n\n\
             Answer with only JSON: {{\"server\":\"...\",\"tool\":\"...\"}} \
             using the exact strings from the list. No prose."
        )
    }

    /// Find the model's choice in the candidate list. Returns the index to
    /// hoist to rank 1, or None if the reply named nothing recognisable —
    /// a model that answers with prose or invents a tool must not be able to
    /// corrupt the ordering.
    pub fn parse_choice(reply: &str, cands: &[Candidate<'_>]) -> Option<usize> {
        let start = reply.find('{')?;
        let end = reply[start..].find('}')? + start + 1;
        let v: serde_json::Value = serde_json::from_str(&reply[start..end]).ok()?;
        let raw_tool = v.get("tool")?.as_str()?.trim();
        let server = v.get("server").and_then(|s| s.as_str()).map(str::trim);
        // Models echo the `server::tool` shape the prompt lists candidates in,
        // so `tool` frequently arrives as "filesystem::write_file" rather than
        // "write_file". Measured against a real model: without this the parse
        // fails and every pick is silently discarded. Accept both forms.
        let (prefix, tool) = match raw_tool.rsplit_once("::") {
            Some((p, t)) => (Some(p), t),
            None => (None, raw_tool),
        };
        // The server may likewise arrive only inside the tool string.
        let server = server.filter(|s| !s.is_empty()).or(prefix);
        // Prefer an exact server+tool match; fall back to tool alone, since a
        // model may omit or mangle the server while naming the tool correctly.
        cands
            .iter()
            .position(|c| Some(c.server) == server && c.tool == tool)
            .or_else(|| cands.iter().position(|c| c.tool == tool))
    }

    /// Ask the model. `sampling` is handled by the caller (it needs the live
    /// MCP peer), so this covers the two transports geco can drive itself.
    /// Returns the index of the chosen candidate.
    pub async fn pick(&self, query: &str, cands: &[Candidate<'_>]) -> Option<usize> {
        let prompt = Self::prompt(query, cands);
        let reply = match &self.transport {
            Transport::Off | Transport::Sampling => return None,
            Transport::Command(argv) => self.run_command(argv, &prompt).await?,
        };
        Self::parse_choice(&reply, cands)
    }

    /// Run the configured command. `{prompt}` anywhere in the arguments is
    /// replaced by the prompt; if the template contains no placeholder the
    /// prompt is appended as a final argument, which is what agent CLIs like
    /// `opencode run "<prompt>"` expect.
    async fn run_command(&self, argv: &[String], prompt: &str) -> Option<String> {
        let (bin, args) = argv.split_first()?;
        let mut cmd = tokio::process::Command::new(bin);
        let mut substituted = false;
        for a in args {
            if a.contains("{prompt}") {
                substituted = true;
                cmd.arg(a.replace("{prompt}", prompt));
            } else {
                cmd.arg(a);
            }
        }
        if !substituted {
            cmd.arg(prompt);
        }
        // A nested agent must not load geco again and recurse into another
        // pick. Sub-sessions are expected to use a stripped profile, and this
        // marker lets a nested geco refuse to serve as a belt-and-braces.
        cmd.env("GECO_NO_PICKER", "1");
        cmd.stdin(std::process::Stdio::null());
        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .ok()?;
        let out = out.ok()?;
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: &str) -> PickerConfig {
        PickerConfig {
            mode: mode.into(),
            ..Default::default()
        }
    }

    fn cands() -> Vec<Candidate<'static>> {
        vec![
            Candidate {
                server: "fs",
                tool: "read_text_file",
                description: "Read a file",
            },
            Candidate {
                server: "gh",
                tool: "create_issue",
                description: "Open an issue",
            },
        ]
    }

    #[test]
    fn default_is_off_so_nobody_pays_for_what_they_did_not_ask_for() {
        let p = Picker::from_config(&PickerConfig::default(), true);
        assert!(!p.is_enabled());
    }

    #[test]
    fn auto_prefers_the_clients_own_model_then_falls_down_the_list() {
        // Sampling available -> use it, it costs the user nothing extra.
        let p = Picker::from_config(&cfg("auto"), true);
        assert_eq!(p.transport(), &Transport::Sampling);

        // No sampling, but a command is configured -> use that.
        let mut c = cfg("auto");
        c.command = vec!["opencode".into(), "run".into()];
        let p = Picker::from_config(&c, false);
        assert!(matches!(p.transport(), Transport::Command(_)));

        // Nothing configured and no sampling -> stay lexical.
        let p = Picker::from_config(&cfg("auto"), false);
        assert_eq!(p.transport(), &Transport::Off);
    }

    #[test]
    fn asking_for_a_transport_that_is_not_available_degrades_instead_of_failing() {
        // `sampling` against a client that cannot do it must not error the
        // search — it must fall back to lexical ranking.
        let p = Picker::from_config(&cfg("sampling"), false);
        assert_eq!(p.transport(), &Transport::Off);
        // Same for a command mode with no command configured.
        let p = Picker::from_config(&cfg("command"), true);
        assert_eq!(p.transport(), &Transport::Off);
    }

    #[test]
    fn parses_a_choice_out_of_a_chatty_reply() {
        let c = cands();
        // Models wrap JSON in prose or fences despite instructions.
        let r = Picker::parse_choice(
            "Sure! ```json\n{\"server\":\"fs\",\"tool\":\"read_text_file\"}\n```",
            &c,
        );
        assert_eq!(r, Some(0));
    }

    #[test]
    fn a_named_tool_still_matches_when_the_server_is_wrong_or_missing() {
        let c = cands();
        assert_eq!(
            Picker::parse_choice(r#"{"tool":"create_issue"}"#, &c),
            Some(1)
        );
        assert_eq!(
            Picker::parse_choice(r#"{"server":"nope","tool":"create_issue"}"#, &c),
            Some(1)
        );
    }

    /// Found by dogfooding, not by imagination: a real model answers
    /// `{"server":"filesystem","tool":"filesystem::write_file"}` because the
    /// prompt lists candidates as `server::tool`. Before this, every such
    /// reply parsed to None and the pick was silently discarded — the picker
    /// looked like it did nothing at all.
    #[test]
    fn accepts_a_tool_that_carries_its_server_prefix() {
        let c = cands();
        assert_eq!(
            Picker::parse_choice(r#"{"server":"fs","tool":"fs::read_text_file"}"#, &c),
            Some(0)
        );
        // Prefix only inside the tool string, no separate server field.
        assert_eq!(
            Picker::parse_choice(r#"{"tool":"gh::create_issue"}"#, &c),
            Some(1)
        );
        // Prefix present but disagreeing with an empty server field.
        assert_eq!(
            Picker::parse_choice(r#"{"server":"","tool":"gh::create_issue"}"#, &c),
            Some(1)
        );
    }

    #[test]
    fn garbage_or_invented_tools_cannot_corrupt_the_ordering() {
        let c = cands();
        assert_eq!(
            Picker::parse_choice("I think you want the file one!", &c),
            None
        );
        assert_eq!(
            Picker::parse_choice(r#"{"server":"fs","tool":"totally_made_up"}"#, &c),
            None
        );
    }

    #[test]
    fn prompt_names_every_candidate_and_demands_json() {
        let c = cands();
        let p = Picker::prompt("read a file", &c);
        assert!(p.contains("read a file"));
        assert!(p.contains("fs::read_text_file"));
        assert!(p.contains("gh::create_issue"));
        assert!(p.contains("only JSON"));
    }
}
