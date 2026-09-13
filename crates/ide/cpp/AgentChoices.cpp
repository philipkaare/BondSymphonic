#include "AgentChoices.h"
#include <QComboBox>

namespace {

/// The label paired with `id` in `list`, or an empty string.
QString labelIn(const QList<agentchoices::Choice>& list, const QString& id) {
    for (const agentchoices::Choice& choice : list) {
        if (QString::fromUtf8(choice.id) == id) {
            return QString::fromUtf8(choice.label);
        }
    }
    return QString();
}

void fill(QComboBox* combo, const QList<agentchoices::Choice>& list) {
    combo->clear();
    for (const agentchoices::Choice& choice : list) {
        combo->addItem(QString::fromUtf8(choice.label), QString::fromUtf8(choice.id));
    }
}

} // namespace

const QList<agentchoices::Choice>& agentchoices::models() {
    static const QList<Choice> kModels{
        { "Default (Claude Code decides)", "" },
        { "Opus 5", "claude-opus-5" },
        { "Sonnet 5", "claude-sonnet-5" },
        { "Haiku 4.5", "claude-haiku-4-5-20251001" },
    };
    return kModels;
}

const QList<agentchoices::Choice>& agentchoices::permissionModes() {
    static const QList<Choice> kModes{
        { "Ask every time", "manual" },
        { "Accept edits", "acceptEdits" },
        { "Plan only", "plan" },
        { "YOLO (sandboxed)", "bypassPermissions" },
    };
    return kModes;
}

QString agentchoices::labelForModel(const QString& id) {
    const QString label = labelIn(models(), id);
    return label.isEmpty() ? id : label;
}

QString agentchoices::labelForPermissionMode(const QString& id) {
    const QString label = labelIn(permissionModes(), id);
    return label.isEmpty() ? id : label;
}

void agentchoices::fillModelCombo(QComboBox* combo, const QString& selected) {
    // Editable, because Claude Code takes any model name and a list that
    // refused one would be this IDE deciding what the CLI supports.
    combo->setEditable(true);
    fill(combo, models());
    const int index = combo->findData(selected);
    if (index >= 0) {
        combo->setCurrentIndex(index);
    } else {
        combo->setEditText(selected);
    }
}

void agentchoices::fillPermissionCombo(QComboBox* combo, const QString& selected) {
    combo->setEditable(false);
    fill(combo, permissionModes());
    const int index = combo->findData(selected);
    // Not the edit text and not an empty combo: an unknown mode falls back to
    // the entry that asks about everything, because every other direction to
    // fall is quieter than what the user last chose.
    combo->setCurrentIndex(index >= 0 ? index : 0);
}

// The offscreen widget checks. See the note in `EditorArea.cpp`; `build.rs`
// defines `BS_WIDGET_TESTS` for every profile but `release`.
#if defined(BS_WIDGET_TESTS)

#include <cstdint>

extern "C" std::int32_t bs_widget_test_agent_choices_are_one_list() {
    if (agentchoices::permissionModes().size() != 4) {
        return 1;
    }
    const QStringList ids{ QStringLiteral("manual"), QStringLiteral("acceptEdits"),
                           QStringLiteral("plan"), QStringLiteral("bypassPermissions") };
    for (int i = 0; i < ids.size(); ++i) {
        if (QString::fromUtf8(agentchoices::permissionModes().at(i).id) != ids.at(i)) {
            return 2;
        }
    }
    for (const agentchoices::Choice& choice : agentchoices::permissionModes()) {
        const QString id = QString::fromUtf8(choice.id);
        // `dontAsk` denies in silence, which is the failure this batch exists
        // to remove rather than to offer as a setting, and `default` is the
        // spelling the IDE stopped using.
        if (id == QLatin1String("default") || id == QLatin1String("dontAsk")) {
            return 3;
        }
    }
    if (agentchoices::labelForPermissionMode(QStringLiteral("bypassPermissions")) !=
        QLatin1String("YOLO (sandboxed)")) {
        return 4;
    }

    QComboBox models;
    agentchoices::fillModelCombo(&models, QStringLiteral("claude-sonnet-5"));
    if (models.count() != 4 || models.currentData().toString() != QLatin1String("claude-sonnet-5")) {
        return 5;
    }
    if (!models.isEditable()) {
        return 6;
    }
    // A model the list has never heard of is shown as the user spelled it.
    QComboBox typed;
    agentchoices::fillModelCombo(&typed, QStringLiteral("claude-something-new"));
    if (typed.currentText() != QLatin1String("claude-something-new")) {
        return 7;
    }

    QComboBox modes;
    agentchoices::fillPermissionCombo(&modes, QStringLiteral("bypassPermissions"));
    if (modes.currentData().toString() != QLatin1String("bypassPermissions")) {
        return 8;
    }
    // An unknown mode falls back to the loudest entry, never to a quieter one.
    QComboBox stale;
    agentchoices::fillPermissionCombo(&stale, QStringLiteral("default"));
    if (stale.currentData().toString() != QLatin1String("manual")) {
        return 9;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
