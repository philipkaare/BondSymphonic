//! Picking the newest model per family out of a `system.list_models` answer,
//! and turning the result into what the model dropdowns show.
//!
//! Pure and Qt-free, like the rest of this module: `system.list_models` is
//! fetched from `qobjects::app_controller`, which hands the reply here and
//! crosses the boundary with whatever this produces. Nothing here talks to the
//! daemon or to Qt.

use bondsymphonic_proto::ModelInfo;
use serde::Serialize;

/// The family an id names: the segment right after `claude-`, up to the next
/// `-` -- `claude-opus-5-5` is `opus`, `claude-sonnet-5` is `sonnet`,
/// `claude-haiku-4-5-20251001` is `haiku`. An id that does not start with
/// `claude-` names no family; see [`newest_per_family`].
fn family(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("claude-")?;
    Some(rest.split('-').next().unwrap_or(rest))
}

/// The newest model of each family in `models`, newest first.
///
/// An id that does not start with `claude-` is not a family this build
/// recognises and is dropped rather than kept under an id-shaped key nobody
/// asked for. "Newest" is decided by comparing `created_at` as a plain string,
/// not by parsing it: every `created_at` the Models API sends is RFC 3339 in
/// one uniform, zero-padded, UTC shape (`2026-09-21T16:24:00Z`), and RFC 3339
/// timestamps in a uniform shape sort the same lexicographically as they do
/// chronologically. `bondsymphonic-ide` carries no time-parsing dependency,
/// and pulling one in for this one comparison is not worth it.
pub fn newest_per_family(models: &[ModelInfo]) -> Vec<ModelInfo> {
    let mut newest: Vec<ModelInfo> = Vec::new();
    for model in models {
        let Some(fam) = family(&model.id) else {
            continue;
        };
        match newest.iter_mut().find(|kept| family(&kept.id) == Some(fam)) {
            Some(kept) if kept.created_at < model.created_at => *kept = model.clone(),
            Some(_) => {}
            None => newest.push(model.clone()),
        }
    }
    newest.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    newest
}

/// The label the model dropdown shows for `display_name`: the API's own
/// "Claude " prefix removed, so "Claude Opus 5.5" reads as "Opus 5.5".
///
/// The model combo is a dock that is routinely narrow enough that a longer
/// label scrolls its editable line edit to the end and shows the tail rather
/// than the name -- see the comment in `AgentChoices.cpp` this mirrors.
fn dropdown_label(display_name: &str) -> &str {
    display_name.strip_prefix("Claude ").unwrap_or(display_name)
}

/// One entry the model dropdowns can be filled with: what `AgentChoices::Choice`
/// needs, and no more. Crosses to C++ as JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
}

/// [`newest_per_family`] turned into what the C++ side fills a combo with.
/// What `qobjects::app_controller` serialises and sends over `modelsChecked`.
pub fn model_choices(models: &[ModelInfo]) -> Vec<ModelChoice> {
    newest_per_family(models)
        .into_iter()
        .map(|m| ModelChoice {
            label: dropdown_label(&m.display_name).to_owned(),
            id: m.id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, display_name: &str, created_at: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: display_name.into(),
            created_at: created_at.into(),
        }
    }

    /// The fixture from the Models-2 brief: eight models across four families
    /// plus one entry that is not a Claude model at all, none of them in any
    /// particular order -- `newest_per_family` has to find the newest of each
    /// family and put the result newest first regardless of the input order.
    fn fixture() -> Vec<ModelInfo> {
        vec![
            model("claude-opus-5", "Claude Opus 5", "2026-06-01T00:00:00Z"),
            model(
                "claude-sonnet-4-5",
                "Claude Sonnet 4.5",
                "2026-02-01T00:00:00Z",
            ),
            model("claude-opus-5-5", "Claude Opus 5.5", "2026-09-21T16:24:00Z"),
            model("claude-haiku-4", "Claude Haiku 4", "2025-01-01T00:00:00Z"),
            model(
                "claude-fable-5-1",
                "Claude Fable 5.1",
                "2026-08-15T00:00:00Z",
            ),
            model("gpt-4", "GPT-4", "2026-09-25T00:00:00Z"),
            model("claude-sonnet-5", "Claude Sonnet 5", "2026-07-01T00:00:00Z"),
            model("claude-fable-5", "Claude Fable 5", "2026-05-01T00:00:00Z"),
            model(
                "claude-haiku-4-5-20251001",
                "Claude Haiku 4.5",
                "2025-10-01T00:00:00Z",
            ),
        ]
    }

    #[test]
    fn newest_per_family_keeps_the_newest_of_each_family_newest_first() {
        assert_eq!(
            newest_per_family(&fixture()),
            vec![
                model("claude-opus-5-5", "Claude Opus 5.5", "2026-09-21T16:24:00Z"),
                model(
                    "claude-fable-5-1",
                    "Claude Fable 5.1",
                    "2026-08-15T00:00:00Z"
                ),
                model("claude-sonnet-5", "Claude Sonnet 5", "2026-07-01T00:00:00Z"),
                model(
                    "claude-haiku-4-5-20251001",
                    "Claude Haiku 4.5",
                    "2025-10-01T00:00:00Z"
                ),
            ]
        );
    }

    /// `gpt-4` is the newest entry in the whole fixture by `created_at`, and it
    /// is still dropped: an id that does not start with `claude-` names no
    /// family this build recognises.
    #[test]
    fn an_id_not_starting_with_claude_is_skipped() {
        let models = vec![model("gpt-4", "GPT-4", "2026-09-25T00:00:00Z")];
        assert_eq!(newest_per_family(&models), Vec::new());
    }

    /// Two entries of the same family: the newer one wins, whichever order
    /// they are listed in.
    #[test]
    fn the_newer_of_two_entries_in_one_family_wins() {
        let older = model("claude-opus-5", "Claude Opus 5", "2026-01-01T00:00:00Z");
        let newer = model("claude-opus-5-5", "Claude Opus 5.5", "2026-09-01T00:00:00Z");
        assert_eq!(
            newest_per_family(&[older.clone(), newer.clone()]),
            vec![newer.clone()]
        );
        assert_eq!(newest_per_family(&[newer.clone(), older]), vec![newer]);
    }

    #[test]
    fn model_choices_strips_the_claude_prefix_and_carries_the_id_through() {
        let models = fixture();
        assert_eq!(
            model_choices(&models),
            vec![
                ModelChoice {
                    id: "claude-opus-5-5".into(),
                    label: "Opus 5.5".into()
                },
                ModelChoice {
                    id: "claude-fable-5-1".into(),
                    label: "Fable 5.1".into()
                },
                ModelChoice {
                    id: "claude-sonnet-5".into(),
                    label: "Sonnet 5".into()
                },
                ModelChoice {
                    id: "claude-haiku-4-5-20251001".into(),
                    label: "Haiku 4.5".into()
                },
            ]
        );
    }

    /// A display name with no "Claude " prefix is shown as it is, rather than
    /// having something stripped off it that was never there.
    #[test]
    fn dropdown_label_leaves_a_name_with_no_claude_prefix_alone() {
        assert_eq!(dropdown_label("Opus 5.5"), "Opus 5.5");
    }

    #[test]
    fn model_choices_of_an_empty_list_is_empty() {
        assert_eq!(model_choices(&[]), Vec::new());
    }
}
