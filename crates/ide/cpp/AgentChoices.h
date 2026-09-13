#pragma once
#include <QList>
#include <QString>

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
struct Choice {
    const char* label;
    const char* id;
};

/// The models offered, in the order they are offered. The first is the empty
/// id, which means "send no `--model` and let Claude Code decide".
const QList<Choice>& models();

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
const QList<Choice>& permissionModes();

/// The sentence shown beside every permission-mode chooser, saying why the
/// modes read the way they do. One string, because three dialogs offering three
/// explanations of the same defect is three chances to leave one behind when it
/// is fixed.
QString permissionNote();

/// The label for an id, or the id itself when the list does not hold it -- a
/// model name the user typed is shown as they typed it.
QString labelForModel(const QString& id);
QString labelForPermissionMode(const QString& id);

/// Fills `combo` with the models, editable, and selects `selected`. An id the
/// list does not hold becomes the edit text rather than being dropped, so a
/// workspace created with a model this build has never heard of still shows it.
void fillModelCombo(QComboBox* combo, const QString& selected);

/// Fills `combo` with the permission modes and selects `selected`, falling back
/// to the first entry -- the one that asks about everything -- for an id the
/// list does not hold. A mode that cannot be shown must never silently become a
/// quieter one.
void fillPermissionCombo(QComboBox* combo, const QString& selected);

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

} // namespace agentchoices
