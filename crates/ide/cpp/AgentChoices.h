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
/// and `dontAsk` denies in silence -- which is indistinguishable from an agent
/// that hung, and an agent that looked hung is the complaint this list was
/// rewritten to answer. `default` is the CLI's undocumented alias for `manual`
/// and is accepted from an older settings file, but only one spelling leaves
/// the IDE and it is the documented one.
const QList<Choice>& permissionModes();

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
inline const char* defaultPermissionMode() {
    return "manual";
}

} // namespace agentchoices
