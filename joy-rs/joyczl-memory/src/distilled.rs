//! Typed extraction contract; malformed results leave the source chat eligible for retry.
use anyhow::{ensure, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct Distillation {
    pub facts: Vec<DurableFact>,
    pub episode: String,
    #[serde(default)]
    pub states: Vec<TemporaryState>,
}

#[derive(Deserialize)]
pub(super) struct DurableFact {
    pub subject: String,
    pub content: String,
    pub kind: Option<String>,
    pub lifetime: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TemporaryState {
    pub subject: String,
    pub content: String,
    pub observation: joyczl_state::facts::TransientObservation,
}

impl Distillation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.facts.len() <= 128 && self.states.len() <= 32,
            "Too many extracted memories"
        );
        for (subject, content) in self
            .facts
            .iter()
            .map(|fact| (&fact.subject, &fact.content))
            .chain(
                self.states
                    .iter()
                    .map(|state| (&state.subject, &state.content)),
            )
        {
            ensure!(
                !subject.trim().is_empty() && subject.chars().count() <= 256,
                "Invalid extracted memory subject"
            );
            ensure!(
                !content.trim().is_empty() && content.chars().count() <= 4096,
                "Invalid extracted memory content"
            );
        }
        ensure!(
            self.episode.chars().count() <= 4096,
            "Extracted episode is too large"
        );
        Ok(())
    }
}
