#pragma once
#include <QList>
#include <QString>
#include <QJsonArray>

class QComboBox;

/// What a Claude agent can be told to be: which model answers, and what it asks
/// before it acts.
///
/// One list of each, because there are three places that offer them -- the New
/// Agent dialog, the composer under an agent's transcript, and the default in
/// Settings -- and three hand-written lists is three chances to drift. They had
/// already drifted: the permission list offered `default` and the model list
/// lived in `NewAgentDialog.cpp` where nothing else could read it.
///
/// Neither list is a gate. Claude Code takes any model name, so the model combo
/// stays editable and an id this file has never heard of goes through
/// untouched; the permission list is closed because the CLI's is.
namespace agentchoices {

/// What the user reads, and what is sent. `label` is for a person and may be
/// rewritten freely; `id` is the CLI's own word and may not.
///
/// `QString`, not `const char*`: the model list can be replaced at runtime
/// with what `system.list_models` fetched, and those strings do not live for
/// the length of the program the way a string literal does. See [`setModels`].
struct Choice {
    QString label;
    QString id;
};

/// The models offered, in the order they are offered. The first is the empty
/// id, which means "send no `--model` and let Claude Code decide".
///
/// A built-in fallback -- `Opus 5.5`, `Fable 5.1`, `Sonnet 5`, `Haiku 4.5` --
/// until [`setModels`] has replaced it with what `system.list_models` fetched;
/// the same fallback comes back if a fetch fails, because [`setModels`] is
/// simply never called then.
const QList<Choice>& models(const QString& backend = QStringLiteral("claude"));

/// Replaces the models offered with `fetched`, keeping the empty "Default"
/// entry first. Called once per connection, when the daemon's
/// `system.list_models` has answered; never on a failure, which leaves
/// whatever `models()` was already returning -- the built-in fallback, or an
/// earlier successful fetch -- in place.
///
/// A combo already on screen when this runs is not told by it: see
/// `AgentArea::refillModels` and `NewAgentDialog`'s own connection to
/// `AppController::modelsChecked` for what refills one, preserving its
/// current selection.
void setModels(const QList<Choice>& fetched);
void setModels(const QString& backend, const QList<Choice>& fetched);
void setDescriptors(const QJsonArray& descriptors);

/// The permission modes offered, in the order they are offered.
///
/// Four of the seven the daemon accepts. `auto` is a mode nobody has asked for,
/// and `dontAsk` denies in silence. `default` is the CLI's undocumented alias
/// for `manual` and is accepted from an older settings file, but only one
/// spelling leaves the IDE and it is the documented one.
///
/// **The labels say what these modes do here, not what their names promise.**
/// Claude Code asks its host before running a tool that needs approval; the
/// daemon claims to be that host (`--permission-prompts host`) and cannot
/// answer, so the CLI refuses instead of asking. A mode called "Ask every time"
/// that silently denies is worse than one that says it denies, so it says it.
/// YOLO is first and is the default, because it is the only one that lets an
/// agent finish a job -- and the sandbox is what makes that reasonable.
const QList<Choice>& permissionModes(const QString& backend = QStringLiteral("claude"));

/// The sentence shown beside every permission-mode chooser, saying why the
/// modes read the way they do. One string, because three dialogs offering three
/// explanations of the same defect is three chances to leave one behind when it
/// is fixed.
QString permissionNote(const QString& backend = QStringLiteral("claude"));

/// The label for an id, or the id itself when the list does not hold it -- a
/// model name the user typed is shown as they typed it.
QString labelForModel(const QString& id, const QString& backend = QStringLiteral("claude"));
QString labelForPermissionMode(const QString& id, const QString& backend = QStringLiteral("claude"));

/// Fills `combo` with the models, editable, and selects `selected`. An id the
/// list does not hold becomes the edit text rather than being dropped, so a
/// workspace created with a model this build has never heard of still shows it.
void fillModelCombo(QComboBox* combo, const QString& selected, const QString& backend = QStringLiteral("claude"));

/// The id `combo` currently stands for: the data behind the label if its text
/// matches one of the list's labels, or the text itself when it does not -- a
/// model typed by hand rather than picked. Also what a refill has to capture
/// before re-filling the same combo, so the choice survives even when the
/// fetch that triggered the refill dropped that id from the list.
QString modelComboSelection(const QComboBox* combo);

/// Fills `combo` with the permission modes and selects `selected`, falling back
/// to the first entry -- the one that asks about everything -- for an id the
/// list does not hold. A mode that cannot be shown must never silently become a
/// quieter one.
void fillPermissionCombo(QComboBox* combo, const QString& selected, const QString& backend = QStringLiteral("claude"));

/// The mode a Claude agent starts on when nothing else has been chosen. The
/// mode is always sent, so this is a floor rather than a fallback.
///
/// `bypassPermissions`, and not because it is the safe end of the range -- it
/// is the loud end. It is the only mode in which an agent can finish a job at
/// all while the host protocol is missing: every other one turns a tool that
/// needs approval into a refusal the user never sees and cannot answer. An
/// agent runs inside a sandbox, in a worktree of its own, behind a network
/// proxy, and that is what makes this defensible where it would not otherwise
/// be. It goes back to `manual` when the CLI can reach this window to ask.
inline const char* defaultPermissionMode() {
    return "bypassPermissions";
}

#if defined(BS_WIDGET_TESTS)
/// Test seam: undoes [`setModels`], so one check's simulated fetch does not
/// leak into the next -- every offscreen widget check but this one assumes
/// `models()` is still the built-in fallback.
void resetModelsForTest();
#endif

} // namespace agentchoices
