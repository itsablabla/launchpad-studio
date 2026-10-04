use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ao_protocol::agent::{CliProviderConfig, OutputFormat};
use ao_protocol::event::AgentEventPayload;
use serde_json::Value;
use tracing::{debug, info};

use crate::helpers;
use crate::traits::OutputNormalizer;

/// Normalizer for the Factory Droid CLI (`droid exec`).
///
/// Handles droid's `stream-json` output (one NDJSON event per line, verified
/// against droid 0.232.0):
///
/// - `{"type":"system","subtype":"init",...}` — carries `session_id`.
/// - `{"type":"message","role":"assistant","id":...,"text":...}` — a complete
///   assistant message, NOT a delta. Droid emits no partial-text events, so
///   each message arrives whole once the model finishes it. The same `id` may
///   repeat with a longer `text` (snapshot growth) or identical `text` (a
///   duplicate emission) — both are deduped against `assistant_text_by_id`.
/// - `{"type":"message","role":"user",...}` — echo of the prompt droid was
///   handed. Suppressed: rendering it would duplicate the user's own message
///   in the chat view.
/// - `{"type":"reasoning","id":...,"text":...}` — complete reasoning block.
///   Emitted twice per block (identical id and text); deduped by id.
/// - `{"type":"tool_call","id":...,"toolName":...,"parameters":{...}}`
/// - `{"type":"tool_result","id":...,"isError":...,"value":...}` — carries no
///   tool name, so the name is recovered from `tool_names_by_id` exactly like
///   Claude's `tool_use_id` lookup.
/// - `{"type":"completion","finalText":...,"usage":{...}}` — terminal event.
///   `usage` uses the canonical Anthropic key names, so
///   `helpers::extract_usage` reads it unchanged. `finalText` is only the
///   LAST assistant message, not the whole turn, so authoritative output text
///   comes from the accumulated `buffer` instead.
pub struct DroidNormalizer {
    output_format: OutputFormat,
    /// Buffers partial lines until a newline arrives (stream modes), or the
    /// whole blob in Json mode.
    line_buffer: String,
    /// Accumulated assistant text across the turn; becomes TextComplete.
    buffer: String,
    session_id: Option<String>,
    session_id_fields: Vec<String>,
    /// tool_call id -> tool name, so the later tool_result (id-only) can be
    /// reported under the real name.
    tool_names_by_id: HashMap<String, String>,
    /// Last raw snapshot per assistant message id, for dedup/suffix logic.
    assistant_text_by_id: HashMap<String, String>,
    /// Last emitted VISIBLE text per assistant message id (raw minus inline
    /// `<thinking>` blocks — see `split_inline_thinking`).
    assistant_visible_by_id: HashMap<String, String>,
    /// Last emitted inline-thinking text per assistant message id.
    assistant_thinking_by_id: HashMap<String, String>,
    /// Reasoning ids already emitted (droid double-emits each block).
    seen_reasoning_ids: HashSet<String>,
    /// Tool-call ids already announced — droid is known to double-emit stream
    /// events (see `seen_reasoning_ids`), and a repeated `tool_call` would
    /// otherwise double-increment `tools_in_flight` (leaking the watchdog
    /// pause) and double-emit ToolCallStarted for the same id.
    seen_tool_call_ids: HashSet<String>,
    /// Per-assistant-message tail withheld by `split_inline_thinking` after an
    /// unclosed `<thinking>` opener. Recomputed from each snapshot; flushed to
    /// the thinking channel by `finalize` if the closer never arrives, so a
    /// truncated final snapshot can't silently destroy reply content.
    assistant_withheld_by_id: HashMap<String, String>,
    /// Set when a thinking channel is open (first reasoning event seen),
    /// consumed to emit ThinkingEnded when the first non-reasoning event
    /// arrives or the stream ends.
    thinking_started_at: Option<std::time::Instant>,
    /// Shared counter the supervisor watches to pause the idle-output
    /// watchdog while a tool call is in flight.
    tools_in_flight: Option<Arc<AtomicUsize>>,
}

impl DroidNormalizer {
    pub fn new(config: &CliProviderConfig) -> Self {
        Self {
            output_format: config.output_format.clone(),
            line_buffer: String::new(),
            buffer: String::new(),
            session_id: None,
            session_id_fields: config.session_id_fields.clone(),
            tool_names_by_id: HashMap::new(),
            assistant_text_by_id: HashMap::new(),
            assistant_visible_by_id: HashMap::new(),
            assistant_thinking_by_id: HashMap::new(),
            seen_reasoning_ids: HashSet::new(),
            seen_tool_call_ids: HashSet::new(),
            assistant_withheld_by_id: HashMap::new(),
            thinking_started_at: None,
            tools_in_flight: None,
        }
    }

    /// If a thinking channel is open, close it and return the end event.
    /// Called before any non-reasoning event is emitted, and on finalize.
    fn close_thinking(&mut self) -> Vec<AgentEventPayload> {
        if let Some(started_at) = self.thinking_started_at.take() {
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            return vec![AgentEventPayload::ThinkingEnded { elapsed_ms }];
        }
        vec![]
    }

    fn capture_session_id(&mut self, value: &Value) {
        if self.session_id.is_none() {
            self.session_id =
                helpers::extract_session_id_from_value(value, &self.session_id_fields);
        }
    }

    /// Split an assistant message snapshot into (visible text, inline
    /// thinking, withheld tail). Some custom-model providers
    /// (generic-chat-completion-api gateways) embed reasoning as
    /// `<thinking>…</thinking>` (or `<think>…`) directly in the message text
    /// instead of producing droid's structured `reasoning` events; routing
    /// those blocks to the thinking channel keeps them out of the reply
    /// bubble. A trailing unclosed opener's tail is returned separately as
    /// `withheld` — droid snapshots grow, so the closer normally lands in a
    /// later snapshot; if the stream ends first, `finalize` flushes the
    /// withheld tail to the thinking channel so no content is destroyed.
    fn split_inline_thinking(raw: &str) -> (String, String, String) {
        const TAGS: [(&str, &str); 2] = [("<thinking>", "</thinking>"), ("<think>", "</think>")];
        let mut visible = String::new();
        let mut thinking = String::new();
        let mut rest = raw;
        loop {
            // Earliest opener of either spelling.
            let Some((open_pos, open_tag, close_tag)) = TAGS
                .iter()
                .filter_map(|(o, c)| rest.find(o).map(|p| (p, *o, *c)))
                .min_by_key(|(p, _, _)| *p)
            else {
                visible.push_str(rest);
                break;
            };
            visible.push_str(&rest[..open_pos]);
            let after_open = &rest[open_pos + open_tag.len()..];
            match after_open.find(close_tag) {
                Some(close_pos) => {
                    thinking.push_str(&after_open[..close_pos]);
                    rest = &after_open[close_pos + close_tag.len()..];
                }
                // Unclosed — hand the tail back for finalize-flush.
                None => return (visible, thinking, after_open.to_string()),
            }
        }
        (visible, thinking, String::new())
    }

    fn process_stream_line(&mut self, line: &str) -> Vec<AgentEventPayload> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        debug!(
            target: "ao_normalizer",
            "[droid] stream event type={:?}, line_len={}",
            event_type,
            trimmed.len()
        );

        let mut events = Vec::new();
        self.capture_session_id(&value);

        match event_type {
            "system" => {
                // init event — session_id already captured above; nothing to render.
            }
            "message" => {
                let role = value.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role != "assistant" {
                    // User-prompt echo — never render as agent output.
                    return events;
                }
                let text = match value.get("text").and_then(|v| v.as_str()) {
                    Some(t) if !t.is_empty() => t,
                    _ => return events,
                };
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // Dedup on the RAW snapshot: an identical re-emission carries
                // no new information, whether visible or thinking.
                if self.assistant_text_by_id.get(&id).is_some_and(|prev| prev == text) {
                    return events;
                }
                self.assistant_text_by_id.insert(id.clone(), text.to_string());

                let (visible, thinking, withheld) = Self::split_inline_thinking(text);
                self.assistant_withheld_by_id.insert(id.clone(), withheld);

                // Inline-thinking delta (gateway-embedded reasoning) routes to
                // the thinking channel exactly like structured `reasoning`.
                let prev_thinking = self
                    .assistant_thinking_by_id
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                let thinking_delta = if thinking == prev_thinking {
                    None
                } else if thinking.starts_with(&prev_thinking) {
                    Some(thinking[prev_thinking.len()..].to_string())
                } else {
                    Some(thinking.clone())
                };
                self.assistant_thinking_by_id.insert(id.clone(), thinking);
                if let Some(delta) = thinking_delta.filter(|d| !d.trim().is_empty()) {
                    if self.thinking_started_at.is_none() {
                        self.thinking_started_at = Some(std::time::Instant::now());
                        events.push(AgentEventPayload::ThinkingStarted);
                    }
                    events.push(AgentEventPayload::ThinkingDelta { text: delta });
                }

                // Visible-text delta: snapshot growth emits only the new
                // suffix; a wholesale replace re-emits the full visible text.
                let prev_visible = self
                    .assistant_visible_by_id
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                let delta = if visible == prev_visible {
                    None
                } else if visible.starts_with(&prev_visible) {
                    Some(visible[prev_visible.len()..].to_string())
                } else {
                    Some(visible.clone())
                };
                self.assistant_visible_by_id.insert(id, visible);

                if let Some(delta) = delta {
                    if !delta.is_empty() {
                        events.extend(self.close_thinking());
                        info!(
                            target: "ao_normalizer",
                            "[droid][assistant] {} chars", delta.len()
                        );
                        self.buffer.push_str(&delta);
                        events.push(AgentEventPayload::TextDelta { text: delta });
                    }
                }
            }
            "reasoning" => {
                let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !id.is_empty() && !self.seen_reasoning_ids.insert(id.to_string()) {
                    // Duplicate emission of a block we already rendered.
                    return events;
                }
                if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                    if self.thinking_started_at.is_none() {
                        self.thinking_started_at = Some(std::time::Instant::now());
                        events.push(AgentEventPayload::ThinkingStarted);
                    }
                    events.push(AgentEventPayload::ThinkingDelta {
                        text: text.to_string(),
                    });
                }
            }
            "tool_call" => {
                let tool_use_id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // Duplicate emission of a call we already announced: skip
                // before touching the watchdog counter or the bus.
                if let Some(id) = &tool_use_id {
                    if !self.seen_tool_call_ids.insert(id.clone()) {
                        return events;
                    }
                }
                events.extend(self.close_thinking());
                let tool_name = value
                    .get("toolName")
                    .or_else(|| value.get("toolId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(id) = &tool_use_id {
                    self.tool_names_by_id.insert(id.clone(), tool_name.clone());
                }
                if let Some(counter) = &self.tools_in_flight {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                info!(target: "ao_normalizer", "[droid][tool_call] {}", tool_name);
                events.push(AgentEventPayload::ToolCallStarted {
                    tool_name,
                    tool_input: value.get("parameters").cloned(),
                    label: None,
                    tool_use_id,
                });
            }
            "tool_result" => {
                let tool_use_id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let tool_name = tool_use_id
                    .as_deref()
                    .and_then(|id| self.tool_names_by_id.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                // `value` is a plain string for shell-style tools but can be a
                // structured object (e.g. file reads) — render either.
                let output = match value.get("value") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => Some(other.to_string()),
                    None => None,
                };
                let is_error = value
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(counter) = &self.tools_in_flight {
                    // Guard against underflow if a tool_result ever arrives
                    // without a matching tool_call.
                    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        if v == 0 {
                            None
                        } else {
                            Some(v - 1)
                        }
                    });
                }
                events.push(AgentEventPayload::ToolCallCompleted {
                    tool_name,
                    output,
                    tool_use_id,
                    is_error,
                });
            }
            "completion" => {
                events.extend(self.close_thinking());
                if let Some(usage) = helpers::extract_usage(&value) {
                    events.push(usage);
                }
                // `finalText` is deliberately NOT used for output: it covers
                // only the last assistant message, while `buffer` holds the
                // whole turn. TextComplete is emitted from `finalize`.
            }
            "error" => {
                // Run-level failure (e.g. session init or auth errors) — the
                // process typically exits nonzero right after. Surface the
                // message on the bus so the failure is visible in the
                // transcript instead of only in backend logs. Field shape is
                // not documented; accept `message` or `error` (string or
                // object) and fall back to the whole event.
                events.extend(self.close_thinking());
                let message = value
                    .get("message")
                    .or_else(|| value.get("error"))
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| value.to_string());
                events.push(AgentEventPayload::Error {
                    message,
                    recoverable: false,
                });
            }
            _ => {}
        }

        events
    }
}

impl OutputNormalizer for DroidNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        match self.output_format {
            OutputFormat::Json => {
                // Single JSON blob at process end — buffer, parse on finalize.
                self.line_buffer.push_str(chunk);
                vec![]
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                self.line_buffer.push_str(chunk);
                let mut events = Vec::new();
                while let Some(newline_pos) = self.line_buffer.find('\n') {
                    let line: String = self.line_buffer.drain(..=newline_pos).collect();
                    events.extend(self.process_stream_line(&line));
                }
                events
            }
            _ => {
                // Text mode — droid's default text output arrives as one blob
                // at process end; pass through like the generic normalizer.
                self.buffer.push_str(chunk);
                vec![AgentEventPayload::TextDelta {
                    text: chunk.to_string(),
                }]
            }
        }
    }

    fn finalize(&mut self, _exit_code: Option<i32>, stderr: &str) -> Vec<AgentEventPayload> {
        let mut events = Vec::new();

        match self.output_format {
            OutputFormat::Json => {
                let buffer = std::mem::take(&mut self.line_buffer);
                if let Ok(value) = serde_json::from_str::<Value>(&buffer) {
                    self.capture_session_id(&value);
                    // droid's `--output-format json` result object carries the
                    // answer in a top-level `result` string, which
                    // `collect_text` reads first.
                    if let Some(text) = helpers::collect_text(&value) {
                        events.push(AgentEventPayload::TextComplete { text });
                    }
                    if let Some(usage) = helpers::extract_usage(&value) {
                        events.push(usage);
                    }
                }
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                if !self.line_buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.line_buffer);
                    events.extend(self.process_stream_line(&remaining));
                }
                // Flush tails withheld behind an unclosed `<thinking>` opener
                // (gateway embedded reasoning whose closer never arrived —
                // stream ended first). Emitting them as thinking keeps the
                // content in the transcript instead of silently destroying
                // it — potentially the bulk of the reply.
                let mut withheld_ids: Vec<&String> = self
                    .assistant_withheld_by_id
                    .iter()
                    .filter(|(_, tail)| !tail.trim().is_empty())
                    .map(|(id, _)| id)
                    .collect();
                withheld_ids.sort();
                for id in withheld_ids {
                    let tail = self
                        .assistant_withheld_by_id
                        .get(id)
                        .cloned()
                        .unwrap_or_default();
                    if self.thinking_started_at.is_none() {
                        self.thinking_started_at = Some(std::time::Instant::now());
                        events.push(AgentEventPayload::ThinkingStarted);
                    }
                    events.push(AgentEventPayload::ThinkingDelta { text: tail });
                }
                events.extend(self.close_thinking());
                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
            _ => {
                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
        }

        if !stderr.is_empty() {
            events.push(AgentEventPayload::Error {
                message: stderr.to_string(),
                recoverable: false,
            });
        }

        events
    }

    fn extract_session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn set_tools_in_flight_counter(&mut self, counter: Arc<AtomicUsize>) {
        self.tools_in_flight = Some(counter);
    }
}
