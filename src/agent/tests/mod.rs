//! Guards the wiring lesson from session 20260903_9255c25e: the prompt the
//! model actually sees is `prompt::SYSTEM_PROMPT`. An earlier fix edited a dead
//! duplicate (src/prompt/mod.rs) and never reached the model — the 65×
//! `list_agents` polling loop in that session was the result. These assertions
//! fail if the fan-in semantics drift out of the live text.

mod prompt;
mod support;
mod wiring;
