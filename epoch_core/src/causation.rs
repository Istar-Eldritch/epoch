//! Utilities for analyzing causal relationships between events.
//!
//! This module provides functions for extracting and navigating the causal
//! tree of events that share a correlation ID. The primary function,
//! [`extract_causation_subtree`], allows you to find all ancestors and
//! descendants of a specific event within a set of correlated events.
//!
//! # Example
//!
//! ```rust
//! use epoch_core::causation::extract_causation_subtree;
//! use epoch_core::event::{Event, EventData};
//! use uuid::Uuid;
//!
//! #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
//! enum MyEvent { Created, Updated }
//!
//! impl EventData for MyEvent {
//!     fn event_type(&self) -> &'static str {
//!         match self {
//!             MyEvent::Created => "Created",
//!             MyEvent::Updated => "Updated",
//!         }
//!     }
//! }
//!
//! // Given a set of correlated events and a target, extract the subtree
//! let subtree = extract_causation_subtree::<MyEvent>(vec![], Uuid::new_v4());
//! assert!(subtree.is_empty());
//! ```

use crate::event::{Event, EventData};
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

/// Extracts the causal subtree from a set of correlated events.
///
/// Given all events sharing a correlation ID and a target event ID, returns:
/// - All ancestors (walking up via `causation_id`)
/// - The target event itself
/// - All descendants (events whose `causation_id` chains lead back to the target)
///
/// Events are returned ordered by `global_sequence`. Events without a
/// `global_sequence` are placed at the end, preserving their relative order.
///
/// Returns an empty `Vec` if `target_event_id` is not found among `events`.
///
/// # Arguments
///
/// * `events` - All events to search through (typically from a single correlation group)
/// * `target_event_id` - The event ID to build the subtree around
///
/// # Example
///
/// ```rust
/// use epoch_core::causation::extract_causation_subtree;
/// use epoch_core::event::{Event, EventData};
/// use uuid::Uuid;
///
/// #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
/// enum MyEvent { Happened }
/// impl EventData for MyEvent {
///     fn event_type(&self) -> &'static str { "Happened" }
/// }
///
/// let events: Vec<Event<MyEvent>> = vec![];
/// let subtree = extract_causation_subtree(events, Uuid::new_v4());
/// assert!(subtree.is_empty());
/// ```
pub fn extract_causation_subtree<D: EventData>(
    events: Vec<Event<D>>,
    target_event_id: Uuid,
) -> Vec<Event<D>> {
    if events.is_empty() {
        return vec![];
    }

    // Step 1: Build index from event ID → position in vec
    let id_to_index: HashMap<Uuid, usize> =
        events.iter().enumerate().map(|(i, e)| (e.id, i)).collect();

    // Check target exists
    let target_idx = match id_to_index.get(&target_event_id) {
        Some(&idx) => idx,
        None => return vec![],
    };

    // Step 2: Build children index (causation_id → indices of events caused by it)
    let mut children: HashMap<Uuid, Vec<usize>> = HashMap::new();
    for (i, event) in events.iter().enumerate() {
        if let Some(causation_id) = event.causation_id {
            children.entry(causation_id).or_default().push(i);
        }
    }

    // Step 3: Collect indices in the subtree
    let mut subtree_indices: HashSet<usize> = HashSet::new();

    // Add the target itself
    subtree_indices.insert(target_idx);

    // Walk up: ancestors via causation_id
    let mut current_idx = target_idx;
    loop {
        let event = &events[current_idx];
        match event.causation_id {
            Some(causation_id) => match id_to_index.get(&causation_id) {
                Some(&parent_idx) => {
                    if !subtree_indices.insert(parent_idx) {
                        break; // Already visited, avoid cycles
                    }
                    current_idx = parent_idx;
                }
                None => break, // Causation ID points outside this event set
            },
            None => break, // Root of the chain
        }
    }

    // Walk down: descendants via BFS
    let mut queue: VecDeque<usize> = VecDeque::new();
    queue.push_back(target_idx);
    while let Some(idx) = queue.pop_front() {
        let event_id = events[idx].id;
        if let Some(child_indices) = children.get(&event_id) {
            for &child_idx in child_indices {
                if subtree_indices.insert(child_idx) {
                    queue.push_back(child_idx);
                }
            }
        }
    }

    // Step 4: Collect events and sort by global_sequence
    let mut result: Vec<(usize, Event<D>)> = subtree_indices
        .into_iter()
        .map(|i| (i, events[i].clone()))
        .collect();

    result.sort_by(|(idx_a, a), (idx_b, b)| {
        match (a.global_sequence, b.global_sequence) {
            (Some(gs_a), Some(gs_b)) => gs_a.cmp(&gs_b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => idx_a.cmp(idx_b), // Preserve insertion order
        }
    });

    result.into_iter().map(|(_, event)| event).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    enum TestEvent {
        Happened { label: String },
    }

    impl EventData for TestEvent {
        fn event_type(&self) -> &'static str {
            "Happened"
        }
    }

    /// Helper to create a test event with specific IDs and optional causation/sequence.
    fn make_event(
        id: Uuid,
        causation_id: Option<Uuid>,
        correlation_id: Option<Uuid>,
        global_sequence: Option<u64>,
        label: &str,
    ) -> Event<TestEvent> {
        Event {
            id,
            stream_id: Uuid::new_v4(),
            stream_version: 1,
            event_type: "Happened".to_string(),
            actor_id: None,
            purger_id: None,
            data: Some(TestEvent::Happened {
                label: label.to_string(),
            }),
            created_at: Utc::now(),
            purged_at: None,
            global_sequence,
            causation_id,
            correlation_id,
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let result = extract_causation_subtree::<TestEvent>(vec![], Uuid::new_v4());
        assert!(result.is_empty());
    }

    #[test]
    fn target_not_found_returns_empty() {
        let event = make_event(Uuid::new_v4(), None, None, Some(1), "A");
        let result = extract_causation_subtree(vec![event], Uuid::new_v4());
        assert!(result.is_empty());
    }

    #[test]
    fn single_event_no_causation() {
        let id = Uuid::new_v4();
        let event = make_event(id, None, None, Some(1), "A");
        let result = extract_causation_subtree(vec![event.clone()], id);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id);
    }

    #[test]
    fn linear_chain_target_middle() {
        // A → B → C, target B → returns [A, B, C]
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(1), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(2), "B");
        let c = make_event(id_c, Some(id_b), Some(corr), Some(3), "C");

        let result = extract_causation_subtree(vec![a, b, c], id_b);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
        assert_eq!(result[2].id, id_c);
    }

    #[test]
    fn linear_chain_target_root() {
        // A → B → C, target A → returns [A, B, C]
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(1), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(2), "B");
        let c = make_event(id_c, Some(id_b), Some(corr), Some(3), "C");

        let result = extract_causation_subtree(vec![a, b, c], id_a);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
        assert_eq!(result[2].id, id_c);
    }

    #[test]
    fn linear_chain_target_leaf() {
        // A → B → C, target C → returns [A, B, C]
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(1), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(2), "B");
        let c = make_event(id_c, Some(id_b), Some(corr), Some(3), "C");

        let result = extract_causation_subtree(vec![a, b, c], id_c);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
        assert_eq!(result[2].id, id_c);
    }

    #[test]
    fn branching_tree_excludes_sibling() {
        // A → B, A → C, target B → returns [A, B] (excludes C)
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(1), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(2), "B");
        let c = make_event(id_c, Some(id_a), Some(corr), Some(3), "C");

        let result = extract_causation_subtree(vec![a, b, c], id_b);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
    }

    #[test]
    fn branching_tree_target_root_includes_all() {
        // A → B, A → C, target A → returns [A, B, C]
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(1), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(2), "B");
        let c = make_event(id_c, Some(id_a), Some(corr), Some(3), "C");

        let result = extract_causation_subtree(vec![a, b, c], id_a);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
        assert_eq!(result[2].id, id_c);
    }

    #[test]
    fn sorted_by_global_sequence() {
        // Events inserted out of order, verify sorted output
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        // Insert in reverse order but with ascending global_sequence
        let c = make_event(id_c, Some(id_b), Some(corr), Some(30), "C");
        let a = make_event(id_a, None, Some(corr), Some(10), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), Some(20), "B");

        let result = extract_causation_subtree(vec![c, a, b], id_a);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, id_a); // gs=10
        assert_eq!(result[1].id, id_b); // gs=20
        assert_eq!(result[2].id, id_c); // gs=30
    }

    #[test]
    fn events_without_global_sequence_preserve_insertion_order() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), None, "A");
        let b = make_event(id_b, Some(id_a), Some(corr), None, "B");
        let c = make_event(id_c, Some(id_b), Some(corr), None, "C");

        let result = extract_causation_subtree(vec![a, b, c], id_a);

        assert_eq!(result.len(), 3);
        // Without global_sequence, original insertion order is preserved
        assert_eq!(result[0].id, id_a);
        assert_eq!(result[1].id, id_b);
        assert_eq!(result[2].id, id_c);
    }

    #[test]
    fn mixed_global_sequence_some_and_none() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let corr = Uuid::new_v4();

        let a = make_event(id_a, None, Some(corr), Some(10), "A");
        let b = make_event(id_b, Some(id_a), Some(corr), None, "B");

        let result = extract_causation_subtree(vec![a, b], id_a);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, id_a); // Has global_sequence, comes first
        assert_eq!(result[1].id, id_b); // No global_sequence, comes after
    }

    #[test]
    fn deep_chain() {
        // A → B → C → D → E, target C → returns all 5
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let corr = Uuid::new_v4();

        let events: Vec<Event<TestEvent>> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let causation = if i == 0 { None } else { Some(ids[i - 1]) };
                make_event(
                    id,
                    causation,
                    Some(corr),
                    Some(i as u64 + 1),
                    &format!("{}", i),
                )
            })
            .collect();

        let result = extract_causation_subtree(events, ids[2]);

        assert_eq!(result.len(), 5);
        for (i, event) in result.iter().enumerate() {
            assert_eq!(event.id, ids[i]);
        }
    }
}
