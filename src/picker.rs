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
//! user is already paying for", so this supports two, tried in preference
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

use rmcp::{Peer, RoleServer};

use crate::config::PickerConfig;

/// Ceiling on the model's reply, for the transports where geco sets it.
///
/// A correct answer is one short JSON object — twenty tokens with room to
/// spare. The ceiling is deliberately far below what deliberation needs, and
/// that is the point rather than a corner cut: a model that ignores the
/// prompt's "do not deliberate" and reasons anyway measured 8% rank-1, *below*
/// the 13.3% lexical baseline. Truncating it into a fallback is the better
/// outcome, so the budget is sized for the answer and not for the thinking.
const MAX_TOKENS: u32 = 512;

/// One thing the model may choose between — a tool or a skill.
pub struct Candidate<'a> {
    /// `"tool"` or `"skill"`. Carried explicitly rather than inferred from an
    /// empty `server`, because a config *can* name a server `""` and a tool
    /// silently rendering as a skill would then be hoisted into the wrong
    /// list. Matches the `kind` string the search response already uses.
    pub kind: &'a str,
    /// Empty for skills, which do not belong to a server.
    pub server: &'a str,
    /// The tool name, or the skill name.
    pub tool: &'a str,
    pub description: &'a str,
}

impl Candidate<'_> {
    fn is_skill(&self) -> bool {
        self.kind == "skill"
    }

    /// How this entry is named in the prompt, and therefore how the model
    /// will name it back.
    fn label(&self) -> String {
        if self.is_skill() {
            format!("skill: {}", self.tool)
        } else {
            format!("{}::{}", self.server, self.tool)
        }
    }
}

/// What one pick attempt achieved.
///
/// The distinction that earns its keep is the last one. A reply geco cannot
/// use is a normal event — fall back to lexical this once and try again on the
/// next search. A transport that *cannot work* is different: a client that
/// advertises sampling and then refuses every request would otherwise cost a
/// round-trip, and often a user-facing approval prompt, on every single search
/// for the life of the session. Once is diagnosis; forever is a bug.
#[derive(Debug, PartialEq)]
pub enum Pick {
    /// Hoist this candidate index to rank 1.
    Choice(usize),
    /// No usable answer. Keep the lexical order; the transport is still fine.
    NoChoice,
    /// The transport failed in a way that will keep failing. Stop asking it.
    TransportDead,
}

/// The one diagnosis a user cannot reach on their own.
///
/// A picker that answers nothing looks exactly like a picker whose model
/// agrees with lexical ranking — same results, just slower. Measured against a
/// real endpoint serving Qwen with reasoning enabled: **7 of 8 searches
/// returned entirely empty content** after spending the whole 512-token budget
/// in a hidden reasoning channel. Neither `/no_think` in the prompt nor
/// `chat_template_kwargs.enable_thinking = false` on the request changed it —
/// whether a model deliberates is decided by the server's chat template, and
/// the client cannot override it.
///
/// So the budget is deliberately not raised to accommodate it. A deliberating
/// model measured **8% rank-1 against lexical's 13.3%**: paying more tokens
/// buys an answer that is worse than not asking. The fix belongs in the
/// serving config, and this message is how the user finds that out.
fn explain_empty_reply() -> &'static str {
    "the model spent its whole output budget without answering. This is what a \
     reasoning model does to a one-line classification. Turn thinking OFF in \
     your serving config — a prompt-level `/no_think` is not reliable, because \
     the chat template decides. Reasoning mode also measured WORSE than no \
     picker at all (8% vs 13.3% rank-1), so raising the budget is not the fix. \
     Falling back to lexical ranking."
}

/// Why a sampling round-trip produced no reply — and, more to the point,
/// whether it is worth trying again.
enum SamplingFailure {
    /// Slow model, or a user who took too long to approve. The next search may
    /// well succeed, so this does not disable anything.
    Transient,
    /// The client answered with a protocol error: method not implemented,
    /// request rejected, malformed. It advertised a capability it will not
    /// honour, and it will not honour it on the next search either.
    Dead,
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
    whole_catalogue_tokens: usize,
    timeout: Duration,
}

impl Picker {
    /// Resolve the configured mode against what is actually available.
    /// `client_supports_sampling` comes from the MCP initialize handshake.
    pub fn from_config(cfg: &PickerConfig, client_supports_sampling: bool) -> Self {
        let has_cmd = !cfg.command.is_empty();
        // Set on every child geco spawns for a pick. Honouring it here is what
        // makes it a guard rather than a decoration: a sub-agent CLI that loads
        // the user's ordinary config would otherwise see `picker.mode:
        // command`, spawn another agent, and recurse — each level bounded only
        // by `timeout_secs`, with unbounded fan-out inside the window. Both the
        // README and the code comment claimed this protection before anything
        // read the variable.
        let nested = std::env::var_os("GECO_NO_PICKER").is_some();
        let transport = match cfg.mode.as_str() {
            _ if nested => Transport::Off,
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
            whole_catalogue_tokens: cfg.whole_catalogue_tokens,
            timeout: Duration::from_secs(cfg.timeout_secs.clamp(1, 300)),
        }
    }

    /// Budget for "does the whole catalogue fit?", in characters.
    ///
    /// Characters rather than tokens because geco has no tokenizer and will
    /// not grow one for this: the ratio varies by model, and the decision is
    /// between two modes that both work, so being off by 20% costs nothing.
    /// `0` means the caller should always use `candidates`.
    pub fn whole_catalogue_budget_chars(&self) -> usize {
        self.whole_catalogue_tokens.saturating_mul(4)
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
    ///
    /// The closing instruction asks the model not to deliberate, and it is
    /// worth asking — but it is a REQUEST, not a control. Whether a model
    /// reasons is decided by the server's chat template: measured against a
    /// real endpoint, neither `/no_think` here nor `enable_thinking: false` on
    /// the request changed anything. What the numbers below compare is two
    /// serving configs, not two prompts. Same weights, same prompt, same
    /// candidates:
    ///
    /// | | rank-1 | precision |
    /// |---|---|---|
    /// | thinking on | 8% | 20% |
    /// | thinking off | **28%** | **70%** |
    ///
    /// It spent 1,125 completion tokens deliberating over a one-line
    /// multiple-choice question and hit the token ceiling before emitting any
    /// JSON. Note the direction: 8% is *below* the 13.3% lexical baseline this
    /// whole feature exists to beat, so a deliberating model is worse than no
    /// picker at all.
    ///
    /// `/no_think` is Qwen's switch specifically; the sentence before it is
    /// the part every other model reads. Both are cheap, and neither can do
    /// harm — a model that does not recognise the token sees a stray word.
    /// The catalogue comes FIRST and the query LAST, and the order is the
    /// whole point rather than a stylistic choice. Prompt caching works on
    /// prefixes: when the candidate list is the same on every search — which
    /// it is in whole-catalogue mode — the expensive part of the prompt is a
    /// stable prefix the server can reuse. Measured against a real endpoint
    /// with a 245-tool catalogue (15,243 prompt tokens):
    ///
    /// | | first call | subsequent | cached |
    /// |---|---|---|---|
    /// | stable prefix | 53s (cold model) | **0.8–1.2s** | **14,727 of 15,243** |
    /// | prefix reordered per call | 12.9s | 12.8–13.0s | 0 |
    ///
    /// **13x faster, and 96.6% of the prompt free.** Putting the query first,
    /// as this did originally, makes every byte of the catalogue follow
    /// variable text and nothing can be reused.
    pub fn prompt(query: &str, cands: &[Candidate<'_>]) -> String {
        format!(
            "{}{}",
            Self::catalogue_block(cands),
            Self::question_block(query, cands.iter().any(Candidate::is_skill))
        )
    }

    /// The cacheable half: identical for a given catalogue, on every search.
    /// Split out from `question_block` so the boundary is explicit and a test
    /// can pin that it does not vary with the query — the day someone
    /// interpolates the query in here, caching silently stops working and
    /// nothing else would notice.
    pub fn catalogue_block(cands: &[Candidate<'_>]) -> String {
        let heading = if cands.iter().any(Candidate::is_skill) {
            "Available tools and skills:"
        } else {
            "Available tools:"
        };
        let list = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "{}. {} — {}",
                    i + 1,
                    c.label(),
                    c.description.chars().take(160).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{heading}\n\n{list}\n\n---\n")
    }

    /// The per-query half. Kept short: everything here is paid in full on
    /// every search, because it can never be part of a reusable prefix.
    ///
    /// With no skills in the pool this is byte-identical to the prompt the
    /// 94.8% measurement was taken with — which is most users, since a skill
    /// library is opt-in. The skill wording appears only when there are
    /// skills to pick, so it cannot silently invalidate that number.
    fn question_block(query: &str, mixed: bool) -> String {
        let (noun, shape) = if mixed {
            (
                "tool or skill",
                "Answer with only JSON: {\"server\":\"...\",\"tool\":\"...\"} \
                 using the exact strings from the list — for a skill, put its \
                 name in \"tool\" and leave \"server\" empty. No prose.",
            )
        } else {
            (
                "tool",
                "Answer with only JSON: {\"server\":\"...\",\"tool\":\"...\"} \
                 using the exact strings from the list. No prose.",
            )
        };
        format!(
            "A user asked: \"{query}\"\n\n\
             Pick the ONE {noun} above that best fulfils that request.\n\
             {shape}\n\
             Answer immediately. This is a lookup, not a reasoning problem: do \
             not deliberate, do not explain, do not weigh the options against \
             each other. /no_think"
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
        // A skill is listed as `skill: name`, so the reply frequently echoes
        // that shape too — the same mistake models make with `server::tool`,
        // which cost an entire release of silently discarded picks. The prefix
        // is also the only signal of WHICH KIND was meant, so it is kept: a
        // skill and a tool can share a name (`changelog`, `review`, `plan` are
        // plausible as both), and resolving `skill: pdf` to a tool called `pdf`
        // would hoist the wrong thing into the wrong list while the log
        // cheerfully reported the wrong kind.
        let (want_kind, raw_tool) = match raw_tool.strip_prefix("skill:") {
            Some(rest) => (Some("skill"), rest.trim()),
            None => (None, raw_tool),
        };
        let (prefix, tool) = match raw_tool.rsplit_once("::") {
            Some((p, t)) => (Some(p), t),
            None => (None, raw_tool),
        };
        // The server may likewise arrive only inside the tool string.
        let server = server.filter(|s| !s.is_empty()).or(prefix);
        // Naming a server is itself evidence of kind: skills do not have one.
        let want_kind = want_kind.or(server.map(|_| "tool"));
        let kind_ok = |c: &Candidate<'_>| want_kind.is_none_or(|k| c.kind == k);
        // Prefer an exact server+tool match; fall back to the name alone, since
        // a model may omit or mangle the server while naming the tool
        // correctly. Both scans respect the kind when one was expressed, and
        // only a scan that finds nothing falls through to ignoring it.
        cands
            .iter()
            .position(|c| Some(c.server) == server && c.tool == tool && kind_ok(c))
            .or_else(|| cands.iter().position(|c| c.tool == tool && kind_ok(c)))
            .or_else(|| cands.iter().position(|c| c.tool == tool))
    }

    /// Ask the model which candidate the request meant.
    ///
    /// `peer` is the live MCP connection back to the client, needed only by
    /// the sampling transport; pass `None` and sampling reports itself dead
    /// rather than retrying against nothing.
    ///
    /// `client` is the harness name from the MCP initialize handshake
    /// (e.g. "opencode", "claude-ai"). Passed to the command transport via
    /// the `GECO_CLIENT` env var so a wrapper script can branch on which
    /// harness is calling and discover its model accordingly. Ignored by
    /// sampling (the harness's own model is used directly) and when off.
    pub async fn pick(
        &self,
        query: &str,
        cands: &[Candidate<'_>],
        peer: Option<&Peer<RoleServer>>,
        client: Option<&str>,
    ) -> Pick {
        let prompt = Self::prompt(query, cands);
        let reply = match &self.transport {
            Transport::Off => return Pick::NoChoice,
            Transport::Sampling => {
                let Some(peer) = peer else {
                    // This transport is only ever selected because the client
                    // advertised sampling at initialize, so a missing peer is
                    // geco's own bug and not the client's. Say so once.
                    tracing::warn!("picker: sampling selected but no peer was stashed");
                    return Pick::TransportDead;
                };
                match self.run_sampling(peer, &prompt).await {
                    Ok(r) => r,
                    Err(SamplingFailure::Transient) => return Pick::NoChoice,
                    Err(SamplingFailure::Dead) => return Pick::TransportDead,
                }
            }
            Transport::Command(argv) => match self.run_command(argv, &prompt, client).await {
                Some(r) => r,
                None => return Pick::NoChoice,
            },
        };
        match Self::parse_choice(&reply, cands) {
            Some(i) => Pick::Choice(i),
            // A reply that names nothing recognisable is the model's problem,
            // not the transport's — the connection worked fine.
            None => Pick::NoChoice,
        }
    }

    /// Borrow the client's own model through `sampling/createMessage`.
    ///
    /// This is the zero-config path: no command to configure, no second model
    /// to pay for, and the model doing the picking is the same one that will
    /// use the result.
    ///
    /// The `allow(deprecated)` is deliberate and load-bearing. SEP-2577
    /// deprecates sampling and rmcp marks the whole type family accordingly,
    /// but nothing has replaced it yet, and it remains the only way an MCP
    /// server can reach a model without shipping its own HTTP and TLS stack —
    /// the exact dependency this crate refuses to take. Confining the `allow`
    /// to this one function means the day a replacement lands, the compiler
    /// points straight at the only code that has to change.
    ///
    /// geco sends one user turn and no system prompt, no temperature and no
    /// model preferences. That is not minimalism for its own sake: those knobs
    /// were not set when the 40%/87% numbers were measured, and asking a
    /// client to route this to a cheaper or faster model would trade away the
    /// model quality the numbers depend on (a 4B model scores 28% where an 80B
    /// scores 40%).
    #[allow(deprecated)]
    async fn run_sampling(
        &self,
        peer: &Peer<RoleServer>,
        prompt: &str,
    ) -> Result<String, SamplingFailure> {
        use rmcp::model::{
            CreateMessageRequestParams, SamplingMessage, SamplingMessageContentBlock,
        };

        let req =
            CreateMessageRequestParams::new(vec![SamplingMessage::user_text(prompt)], MAX_TOKENS);
        let res = tokio::time::timeout(self.timeout, peer.create_message(req))
            .await
            .map_err(|_| {
                tracing::debug!("picker: sampling timed out after {:?}", self.timeout);
                SamplingFailure::Transient
            })?
            .map_err(|e| {
                tracing::warn!("picker: the client refused sampling/createMessage: {e}");
                SamplingFailure::Dead
            })?;

        // Truncation is worth naming precisely, because its symptom is silence
        // and its cause is unguessable from the outside. Observed on a real
        // endpoint: a reasoning model spent the ENTIRE budget in a hidden
        // reasoning channel and returned empty content with `maxTokens` — so
        // every search paid a full round-trip and several seconds to produce
        // nothing, indistinguishable from a picker that simply agrees with the
        // lexical order. See `explain_empty_reply` for why the budget is not
        // just raised instead.
        let truncated = res.stop_reason.as_deref() == Some("maxTokens");
        // The reply may be a single block or several; only text can carry a
        // choice, so image and audio blocks are dropped rather than described.
        let text = res
            .message
            .content
            .into_vec()
            .into_iter()
            .filter_map(|b| match b {
                SamplingMessageContentBlock::Text(t) => Some(t.text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if truncated && text.trim().is_empty() {
            tracing::warn!("picker: {}", explain_empty_reply());
        }
        Ok(text)
    }

    /// Run the configured command. `{prompt}` anywhere in the arguments is
    /// replaced by the prompt; if the template contains no placeholder the
    /// prompt is appended as a final argument, which is what agent CLIs like
    /// `opencode run "<prompt>"` expect.
    ///
    /// `client` is the harness name from the MCP handshake, exposed to the
    /// command via `GECO_CLIENT` so a wrapper script can branch on which
    /// harness is calling and discover its model the right way — each
    /// harness stores its session model differently.
    async fn run_command(
        &self,
        argv: &[String],
        prompt: &str,
        client: Option<&str>,
    ) -> Option<String> {
        let (bin, args) = argv.split_first()?;
        // `~` is a shell convention, and there is no shell here — the binary
        // is handed straight to execvp. The config the installer scaffolds
        // says `command: ["~/.config/geco/picker/pick.sh", …]`, which spawned
        // nothing at all: every search logged one ENOENT and fell back to the
        // lexical order, while the miss-streak advice blamed the model. Expand
        // it the same way skills dirs are expanded. A bare PATH name like
        // `opencode` has no `~/` prefix, so it comes back untouched and still
        // resolves through PATH.
        let mut cmd = tokio::process::Command::new(crate::skills::expand_tilde(bin));
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
        // Tell the command which harness is calling, so a wrapper script can
        // discover the model the harness-specific way. The MCP handshake is
        // the only reliable source — guessing from env vars or paths would
        // break the moment two harnesses share a machine.
        if let Some(name) = client {
            cmd.env("GECO_CLIENT", name);
        }
        cmd.stdin(std::process::Stdio::null());
        // Without this, a timeout leaks the process. `timeout` only drops the
        // future; tokio does NOT reap a child on drop unless told to. The
        // command here is typically an agent CLI, so the abandoned process
        // runs its inference to completion, bills the user's account for an
        // answer nobody will read, and holds a rate-limit slot. A session of
        // searches that each run slightly over the bound would accumulate one
        // orphan apiece, invisible to geco, while the picker logs nothing
        // worse than a clean fallback.
        cmd.kill_on_drop(true);
        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| tracing::debug!("picker: command timed out after {:?}", self.timeout))
            .ok()?;
        let out = out
            .map_err(|e| tracing::warn!("picker: cannot run the configured command: {e}"))
            .ok()?;
        if !out.status.success() {
            // A wrapper script that exits non-zero has failed, and its stdout
            // is not an answer. Say so: the alternative is a picker that looks
            // like it is working while every pick silently falls back, which
            // is the exact failure this feature has shipped twice already.
            tracing::warn!(
                "picker: command exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            );
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        // A command that succeeds and prints nothing has the same cause and
        // the same invisible symptom as a truncated sampling reply — most
        // often an agent CLI whose model reasoned past its output budget.
        if text.trim().is_empty() {
            tracing::warn!("picker: {}", explain_empty_reply());
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_config` reads `GECO_NO_PICKER`, and cargo runs tests as threads in
    /// one process — so a test that sets it would otherwise flip the transport
    /// out from under any other test calling `from_config` at that moment.
    /// Every such test takes this first. (Caught by exactly that flake.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test poisons the lock; the data is (), so recover.
        let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("GECO_NO_PICKER");
        g
    }

    fn cfg(mode: &str) -> PickerConfig {
        PickerConfig {
            mode: mode.into(),
            ..Default::default()
        }
    }

    fn cands() -> Vec<Candidate<'static>> {
        vec![
            Candidate {
                kind: "tool",
                server: "fs",
                tool: "read_text_file",
                description: "Read a file",
            },
            Candidate {
                kind: "tool",
                server: "gh",
                tool: "create_issue",
                description: "Open an issue",
            },
        ]
    }

    /// Tools and skills in one pool, which is how the picker actually sees
    /// them: one question, one answer, of either kind.
    fn mixed_cands() -> Vec<Candidate<'static>> {
        let mut c = cands();
        c.push(Candidate {
            kind: "skill",
            server: "",
            tool: "plan-a-project",
            description: "Break a goal into steps",
        });
        c
    }

    #[test]
    fn default_is_off_so_nobody_pays_for_what_they_did_not_ask_for() {
        let _env = env_guard();
        let p = Picker::from_config(&PickerConfig::default(), true);
        assert!(!p.is_enabled());
    }

    #[test]
    fn auto_prefers_the_clients_own_model_then_falls_down_the_list() {
        let _env = env_guard();
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
        let _env = env_guard();
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

    /// The catalogue half must not vary with the query, or prompt caching
    /// stops working — silently, because the prompt still reads correctly and
    /// the answers stay right. Only the bill and the latency change, and
    /// neither is visible from a test that checks output. Measured: a stable
    /// prefix served 96.6% of a 15k-token prompt from cache and ran 13x
    /// faster than the same prompt reordered per call.
    #[test]
    fn the_catalogue_half_of_the_prompt_never_varies_with_the_query() {
        let c = cands();
        let a = Picker::catalogue_block(&c);
        let b = Picker::catalogue_block(&c);
        assert_eq!(a, b, "the same catalogue must render identically");

        // The whole prompt differs by query; the cacheable prefix must not.
        let p1 = Picker::prompt("read a file", &c);
        let p2 = Picker::prompt("open an issue on github", &c);
        assert_ne!(p1, p2, "the query must reach the model somewhere");
        assert!(p1.starts_with(&a), "the catalogue must lead: {p1}");
        assert!(p2.starts_with(&a), "the catalogue must lead: {p2}");
        assert!(
            !a.contains("read a file") && !a.contains("github"),
            "no query text may leak into the cacheable prefix: {a}"
        );
    }

    /// Skills are listed and named differently from tools, and both halves of
    /// that have to work: they must render without a meaningless `::` prefix,
    /// and a reply echoing the `skill:` label back must still resolve. Models
    /// echoing the prompt's own shape is not hypothetical — it cost an entire
    /// release of silently discarded picks with `server::tool`.
    #[test]
    fn skills_are_listed_and_named_without_a_server() {
        let c = mixed_cands();
        let block = Picker::catalogue_block(&c);
        assert!(block.contains("skill: plan-a-project"), "{block}");
        assert!(
            !block.contains("::plan-a-project"),
            "a skill has no server to prefix with: {block}"
        );
        assert!(block.contains("Available tools and skills:"), "{block}");

        // Bare name, the label echoed back, and an empty server field.
        for reply in [
            r#"{"tool":"plan-a-project"}"#,
            r#"{"tool":"skill: plan-a-project"}"#,
            r#"{"server":"","tool":"plan-a-project"}"#,
        ] {
            assert_eq!(
                Picker::parse_choice(reply, &c),
                Some(2),
                "must resolve the skill: {reply}"
            );
        }
    }

    /// A skill and a tool can share a name — `changelog`, `review`, `plan` are
    /// plausible as either. Resolving a picked skill to a same-named tool would
    /// hoist the wrong entry into the wrong list, and the log line would report
    /// the wrong kind, so nothing on the operator side would show it.
    #[test]
    fn a_named_kind_is_not_resolved_to_the_other_kinds_namesake() {
        let cands = vec![
            Candidate {
                kind: "tool",
                server: "docs",
                tool: "review",
                description: "a tool",
            },
            Candidate {
                kind: "skill",
                server: "",
                tool: "review",
                description: "a skill",
            },
        ];
        assert_eq!(
            Picker::parse_choice(r#"{"tool":"skill: review"}"#, &cands),
            Some(1),
            "the `skill:` label must select the skill, not the tool"
        );
        assert_eq!(
            Picker::parse_choice(r#"{"server":"docs","tool":"review"}"#, &cands),
            Some(0),
            "naming a server must select the tool"
        );
        // No kind expressed at all: either is defensible, and the old
        // first-match behaviour is kept rather than invented.
        assert_eq!(
            Picker::parse_choice(r#"{"tool":"review"}"#, &cands),
            Some(0)
        );
    }

    /// The recursion guard has to be read, not just written. A sub-agent CLI
    /// loading the user's ordinary config would otherwise see the picker
    /// enabled and spawn another agent, each level bounded only by the timeout.
    #[test]
    fn a_nested_geco_refuses_to_pick() {
        let _env = env_guard();
        let mut c = cfg("command");
        c.command = vec!["some-agent".into()];
        assert!(Picker::from_config(&c, false).is_enabled());

        std::env::set_var("GECO_NO_PICKER", "1");
        let nested = Picker::from_config(&c, true);
        std::env::remove_var("GECO_NO_PICKER");
        assert!(
            !nested.is_enabled(),
            "a picker spawned by a picker must not pick again"
        );
    }

    /// A skill library is opt-in, so most users never have one — and for them
    /// the prompt must stay exactly what the 94.8% measurement was taken with.
    /// Adding skills support must not silently re-scope a published number.
    #[test]
    fn a_tools_only_prompt_is_unchanged_by_skills_support() {
        let p = Picker::prompt("read a file", &cands());
        assert!(p.contains("Available tools:\n"), "{p}");
        assert!(p.contains("Pick the ONE tool above"), "{p}");
        assert!(
            !p.contains("skill"),
            "no skill wording may appear when there are no skills: {p}"
        );

        // With skills present, the instruction has to say so — otherwise the
        // model is told to pick a "tool" from a list containing skills.
        let m = Picker::prompt("read a file", &mixed_cands());
        assert!(m.contains("Pick the ONE tool or skill above"), "{m}");
    }

    /// Worth more than any ranking change measured on this corpus: telling the
    /// model not to reason took the same weights from 8% rank-1 to 28%. Losing
    /// this line would look like nothing and cost more than everything.
    #[test]
    fn prompt_tells_the_model_not_to_deliberate() {
        let p = Picker::prompt("read a file", &cands());
        assert!(p.contains("/no_think"), "Qwen's switch: {p}");
        assert!(
            p.contains("not a reasoning problem"),
            "the instruction every other model reads: {p}"
        );
    }

    #[tokio::test]
    async fn sampling_without_a_peer_is_dead_rather_than_retried() {
        // Scoped: the guard is a std Mutex and must not be held across the
        // await below. Only the construction reads the environment.
        let p = {
            let _env = env_guard();
            Picker::from_config(&cfg("sampling"), true)
        };
        assert_eq!(p.transport(), &Transport::Sampling);
        // Selecting sampling means the client advertised it, so arriving here
        // without a peer is geco's own bug. Reporting it dead makes it show up
        // once in the log instead of once per search, forever.
        assert_eq!(p.pick("q", &cands(), None, None).await, Pick::TransportDead);
    }

    /// A command that will not run is NOT treated as a dead transport, unlike
    /// a refused sampling request. The asymmetry is deliberate: a failed spawn
    /// costs microseconds and no user interaction, and config hot-reload can
    /// fix the path under a running proxy — so retrying is nearly free and
    /// sometimes right. A refused sampling request costs a full round-trip and
    /// often an approval dialog.
    #[tokio::test]
    async fn a_command_that_cannot_run_leaves_the_transport_alive() {
        let mut c = cfg("command");
        c.command = vec!["geco-no-such-binary-anywhere".into()];
        let p = {
            let _env = env_guard();
            Picker::from_config(&c, false)
        };
        assert!(matches!(p.transport(), Transport::Command(_)));
        assert_eq!(p.pick("q", &cands(), None, None).await, Pick::NoChoice);
    }

    #[tokio::test]
    async fn a_disabled_picker_never_reports_a_choice() {
        let p = {
            let _env = env_guard();
            Picker::from_config(&PickerConfig::default(), true)
        };
        assert_eq!(p.pick("q", &cands(), None, None).await, Pick::NoChoice);
    }
}
