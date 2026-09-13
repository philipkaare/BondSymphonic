#include "AgentChoices.h"
#include <QComboBox>
#include <QLineEdit>

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
        // Just "Default". The pane is a dock and is routinely narrow enough
        // that a longer label scrolls its editable line edit to the end and
        // shows the user "ode decides)", which is worse than saying less. The
        // combo's tooltip carries the meaning.
        { "Default", "" },
        { "Opus 5", "claude-opus-5" },
        { "Sonnet 5", "claude-sonnet-5" },
        { "Haiku 4.5", "claude-haiku-4-5-20251001" },
    };
    return kModels;
}

const QList<agentchoices::Choice>& agentchoices::permissionModes() {
    // The labels say what these modes DO here, which is not what their names
    // promise. Claude Code asks its host before running a tool that needs
    // approval, and the daemon passes `--permission-prompts host` without ever
    // becoming a host that can answer -- so the CLI resolves the question by
    // refusing. Nothing reaches the amber bar, and a mode called "Ask every
    // time" that silently denies is worse than one that says it denies.
    //
    // See `docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md`.
    // When the host protocol lands these go back to their plain names.
    static const QList<Choice> kModes{
        { "YOLO (sandboxed)", "bypassPermissions" },
        { "Accept edits (other tools blocked)", "acceptEdits" },
        { "Plan only", "plan" },
        { "Ask every time (blocks instead)", "manual" },
    };
    return kModes;
}

QString agentchoices::permissionNote() {
    return QStringLiteral(
        "Claude Code cannot reach this window to ask, so a tool that needs approval is refused "
        "rather than queued. YOLO runs everything — the sandbox, the worktree and the network "
        "proxy are what make that reasonable.");
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
    // Wound back to the start. A line edit narrower than its text keeps
    // whichever end the cursor is at, and a model combo that has just been
    // filled shows the tail -- "…5-20251001" -- which names nothing.
    if (QLineEdit* edit = combo->lineEdit()) {
        edit->setCursorPosition(0);
    }
}

void agentchoices::fillPermissionCombo(QComboBox* combo, const QString& selected) {
    combo->setEditable(false);
    fill(combo, permissionModes());
    const int index = combo->findData(selected);
    if (index >= 0) {
        combo->setCurrentIndex(index);
        return;
    }
    // An unknown mode falls back to the most restrictive entry by name, never
    // to index 0 -- index 0 is YOLO now, and a settings file this build cannot
    // read is not consent to run every tool unasked. Restrictive here means
    // "refuses", which is useless but is the user's to discover rather than
    // ours to decide for them.
    const int manual = combo->findData(QStringLiteral("manual"));
    combo->setCurrentIndex(manual >= 0 ? manual : 0);
}

// The offscreen widget checks. See the note in `EditorArea.cpp`; `build.rs`
// defines `BS_WIDGET_TESTS` for every profile but `release`.
#if defined(BS_WIDGET_TESTS)

#include <cstdint>

extern "C" std::int32_t bs_widget_test_agent_choices_are_one_list() {
    if (agentchoices::permissionModes().size() != 4) {
        return 1;
    }
    // YOLO first, because it is the default and the only mode an agent can
    // finish a job in while the CLI cannot reach this window to ask.
    const QStringList ids{ QStringLiteral("bypassPermissions"), QStringLiteral("acceptEdits"),
                           QStringLiteral("plan"), QStringLiteral("manual") };
    for (int i = 0; i < ids.size(); ++i) {
        if (QString::fromUtf8(agentchoices::permissionModes().at(i).id) != ids.at(i)) {
            return 2;
        }
    }
    if (QString::fromUtf8(agentchoices::defaultPermissionMode()) !=
        QLatin1String("bypassPermissions")) {
        return 10;
    }
    // Every mode whose name promises a prompt says in its label that it does
    // not give one. The day the host protocol lands, this is the assertion that
    // says the labels have to go back to their plain names.
    for (const agentchoices::Choice& choice : agentchoices::permissionModes()) {
        const QString id = QString::fromUtf8(choice.id);
        const QString label = QString::fromUtf8(choice.label);
        if ((id == QLatin1String("manual") || id == QLatin1String("acceptEdits")) &&
            !label.contains(QLatin1String("block"))) {
            return 11;
        }
    }
    if (!agentchoices::permissionNote().contains(QLatin1String("refused"))) {
        return 12;
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
    // An unknown mode falls back to the one that refuses, never to YOLO: a
    // settings file this build cannot read is not consent to run every tool.
    QComboBox stale;
    agentchoices::fillPermissionCombo(&stale, QStringLiteral("default"));
    if (stale.currentData().toString() != QLatin1String("manual")) {
        return 9;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
