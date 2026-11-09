//! Call-local message numbers for one compression request.
//!
//! The compressor cites messages by short numbers (`m1`, `m2`, ...) instead of
//! copying native message ids. The numbering is a pure function of the native
//! compression input, so format retries and resume reproduce the same mapping
//! without persisting it. Persisted checkpoints keep native ids.

use std::collections::HashMap;

use super::{CompressionInputV1, ContextSummaryV1, SummaryFactV1};

pub(super) struct MessageNumbers {
    native_to_number: HashMap<String, String>,
}

impl MessageNumbers {
    /// Numbers allowed source ids first so citable messages get the lowest
    /// numbers, then every other message id the model can see.
    pub(super) fn of(input: &CompressionInputV1) -> Self {
        let range = &input.summarize_only_range;
        let mut numbers = Self {
            native_to_number: HashMap::new(),
        };
        for id in range
            .allowed_source_message_ids
            .iter()
            .chain([&range.from_message_id, &range.through_message_id])
            .chain(
                input
                    .summarize_prefix
                    .iter()
                    .map(|message| &message.message_id),
            )
            .chain(
                input
                    .continuation_lens
                    .iter()
                    .map(|message| &message.message_id),
            )
            .chain(
                input
                    .prior_checkpoint
                    .iter()
                    .flat_map(ContextSummaryV1::all_facts)
                    .flat_map(|fact| fact.source_message_ids.iter()),
            )
        {
            numbers.assign(id);
        }
        numbers
    }

    fn assign(&mut self, native: &str) {
        let next = self.native_to_number.len() + 1;
        self.native_to_number
            .entry(native.to_owned())
            .or_insert_with(|| format!("m{next}"));
    }

    fn number(&self, native: &str) -> String {
        self.native_to_number[native].clone()
    }

    /// Returns the model-facing copy of the input with every message id numbered.
    pub(super) fn project(&self, input: &CompressionInputV1) -> CompressionInputV1 {
        let mut projected = input.clone();
        for message in projected
            .summarize_prefix
            .iter_mut()
            .chain(projected.continuation_lens.iter_mut())
        {
            message.message_id = self.number(&message.message_id);
        }
        let range = &mut projected.summarize_only_range;
        range.from_message_id = self.number(&range.from_message_id);
        range.through_message_id = self.number(&range.through_message_id);
        for id in &mut range.allowed_source_message_ids {
            *id = self.number(id);
        }
        if let Some(prior) = &mut projected.prior_checkpoint {
            for fact in prior.all_facts_mut() {
                for id in &mut fact.source_message_ids {
                    *id = self.number(id);
                }
            }
        }
        projected
    }

    /// Restores native ids in a model summary. Unknown numbers stay as given so
    /// summary validation rejects them like any other unknown source id.
    pub(super) fn restore(&self, summary: &mut ContextSummaryV1) {
        let number_to_native = self
            .native_to_number
            .iter()
            .map(|(native, number)| (number.as_str(), native.as_str()))
            .collect::<HashMap<_, _>>();
        for fact in summary.all_facts_mut() {
            for id in &mut fact.source_message_ids {
                if let Some(native) = number_to_native.get(id.as_str()) {
                    *id = (*native).to_owned();
                }
            }
        }
    }
}

impl ContextSummaryV1 {
    fn all_facts_mut(&mut self) -> impl Iterator<Item = &mut SummaryFactV1> {
        [
            &mut self.goals,
            &mut self.historical_constraints,
            &mut self.reported_observations,
            &mut self.completed_actions,
            &mut self.unresolved_questions,
            &mut self.next_steps,
            &mut self.important_identifiers,
            &mut self.omitted_evidence,
        ]
        .into_iter()
        .flatten()
    }
}
