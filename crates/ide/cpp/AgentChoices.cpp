#include "AgentChoices.h"
#include <QComboBox>
#include <QLineEdit>
#include <QMap>
#include <QJsonObject>

namespace {

/// The label paired with `id` in `list`, or an empty string.
QString labelIn(const QList<agentchoices::Choice>& list, const QString& id) {
    for (const agentchoices::Choice& choice : list) {
        if (choice.id == id) {
            return choice.label;
        }
    }
    return QString();
}

void fill(QComboBox* combo, const QList<agentchoices::Choice>& list) {
    combo->clear();
    for (const agentchoices::Choice& choice : list) {
        combo->addItem(choice.label, choice.id);
    }
}

/// What `system.list_models` fetched, once a fetch has succeeded -- empty
/// until then, and again after [`resetModelsForTest`]. Kept apart from the
/// fallback below rather than overwriting it, so a fetch that never comes back
/// (an old daemon, one with no Claude credentials) leaves the fallback in
/// place rather than an empty combo.
QMap<QString, QList<agentchoices::Choice>> backendModels;
QMap<QString, QList<agentchoices::Choice>> backendModes;
QMap<QString, QString> backendNotes;
QList<agentchoices::Choice>& fetchedModels() { return backendModels[QStringLiteral("claude")]; }

/// Whether [`fetchedModels`] holds a real answer. A plain `bool` rather than
/// `fetchedModels().isEmpty()`, because "Default" alone -- a fetch that came
/// back with no models in it -- is a real, if unlikely, answer too.
bool& hasFetchedModels() {
    static bool has = false;
    return has;
}

} // namespace

const QList<agentchoices::Choice>& agentchoices::models(const QString& backend) {
    if (backendModels.contains(backend)) return backendModels[backend];
    static const QList<Choice> defaultOnly{{QStringLiteral("Default"), QString()}};
    if (backend != QLatin1String("claude")) return defaultOnly;
    // The built-in fallback: what a dropdown shows until the first
    // `system.list_models` answers, and what it goes back to showing if a
    // fetch fails. Kept in the shape `newest_per_family` -- the daemon-side
    // and IDE-side selection this fallback stands in for -- delivers it: the
    // newest model of each family, newest first, labelled with `display_name`
    // less its "Claude " prefix.
    static const QList<Choice> kFallback{
        // Just "Default". The pane is a dock and is routinely narrow enough
        // that a longer label scrolls its editable line edit to the end and
        // shows the user "ode decides)", which is worse than saying less. The
        // combo's tooltip carries the meaning.
        { QStringLiteral("Default"), QStringLiteral("") },
        { QStringLiteral("Opus 5.5"), QStringLiteral("claude-opus-5-5") },
        { QStringLiteral("Fable 5.1"), QStringLiteral("claude-fable-5-1") },
        { QStringLiteral("Sonnet 5"), QStringLiteral("claude-sonnet-5") },
        { QStringLiteral("Haiku 4.5"), QStringLiteral("claude-haiku-4-5-20251001") },
    };
    return hasFetchedModels() ? fetchedModels() : kFallback;
}

void agentchoices::setModels(const QList<Choice>& fetched) {
    setModels(QStringLiteral("claude"), fetched);
}

void agentchoices::setModels(const QString& backend, const QList<Choice>& fetched) {
    QList<Choice>& storage = backendModels[backend];
    storage.clear();
    storage.append({ QStringLiteral("Default"), QStringLiteral("") });
    storage += fetched;
    hasFetchedModels() = true;
}

const QList<agentchoices::Choice>& agentchoices::permissionModes(const QString& backend) {
    if (backendModes.contains(backend)) return backendModes[backend];
    static const QList<Choice> empty;
    if (backend != QLatin1String("claude")) return empty;
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

QString agentchoices::permissionNote(const QString& backend) {
    if (backendNotes.contains(backend)) return backendNotes[backend];
    if (backend != QLatin1String("claude")) return QString();
    return QStringLiteral(
        "Claude Code cannot reach this window to ask, so a tool that needs approval is refused "
        "rather than queued. YOLO runs everything — the sandbox, the worktree and the network "
        "proxy are what make that reasonable.");
}

QString agentchoices::labelForModel(const QString& id, const QString& backend) {
    const QString label = labelIn(models(backend), id);
    return label.isEmpty() ? id : label;
}

QString agentchoices::labelForPermissionMode(const QString& id, const QString& backend) {
    const QString label = labelIn(permissionModes(backend), id);
    return label.isEmpty() ? id : label;
}

void agentchoices::fillModelCombo(QComboBox* combo, const QString& selected, const QString& backend) {
    // Editable, because Claude Code takes any model name and a list that
    // refused one would be this IDE deciding what the CLI supports.
    combo->setEditable(true);
    fill(combo, models(backend));
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

QString agentchoices::modelComboSelection(const QComboBox* combo) {
    const QString shown = combo->currentText().trimmed();
    // A label off the list stands for the id behind it -- nobody types
    // "Opus 5.5" at a CLI -- and anything else is an id typed by hand.
    const int listed = combo->findText(shown);
    return listed >= 0 ? combo->itemData(listed).toString() : shown;
}

void agentchoices::fillPermissionCombo(QComboBox* combo, const QString& selected, const QString& backend) {
    combo->setEditable(false);
    fill(combo, permissionModes(backend));
    const QString mode = backend == "claude" && selected == "default" ? QStringLiteral("manual") : selected;
    const int index = combo->findData(mode);
    if (index >= 0) {
        combo->setCurrentIndex(index);
        return;
    }
    // An unknown mode falls back to the most restrictive entry by name, never
    // to index 0 -- index 0 is YOLO now, and a settings file this build cannot
    // read is not consent to run every tool unasked. Restrictive here means
    // "refuses", which is useless but is the user's to discover rather than
    // ours to decide for them.
    if (!mode.isEmpty()) { combo->addItem(mode, mode); combo->setCurrentIndex(combo->count()-1); return; }
    const int manual = combo->findData(backend == QLatin1String("claude") ? QStringLiteral("manual") : QStringLiteral("on-request"));
    combo->setCurrentIndex(manual >= 0 ? manual : 0);
}

void agentchoices::setDescriptors(const QJsonArray& descriptors) {
    backendModes.clear(); backendNotes.clear();
    for (const auto& value : descriptors) {
        const auto descriptor=value.toObject();
        const auto id=descriptor.value("id").toString();
        QList<Choice> modes;
        for (const auto& entry : descriptor.value("permission_modes").toArray()) {
            const auto mode=entry.toObject();
            modes.append({mode.value("label").toString(),mode.value("id").toString()});
        }
        backendModes.insert(id,modes);
        backendNotes.insert(id,descriptor.value("permission_note").toString());
    }
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
        if (agentchoices::permissionModes().at(i).id != ids.at(i)) {
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
        const QString& id = choice.id;
        const QString& label = choice.label;
        if ((id == QLatin1String("manual") || id == QLatin1String("acceptEdits")) &&
            !label.contains(QLatin1String("block"))) {
            return 11;
        }
    }
    if (!agentchoices::permissionNote().contains(QLatin1String("refused"))) {
        return 12;
    }
    for (const agentchoices::Choice& choice : agentchoices::permissionModes()) {
        const QString& id = choice.id;
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

    // "Default" plus the built-in fallback: the newest model of each family
    // as of this writing, which is also what `newest_per_family`'s own fixture
    // test expects the daemon to answer with today.
    QComboBox models;
    agentchoices::fillModelCombo(&models, QStringLiteral("claude-sonnet-5"));
    if (models.count() != 5 || models.currentData().toString() != QLatin1String("claude-sonnet-5")) {
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

    // A fetch landing replaces the list; a combo already showing a choice that
    // is still on the new list keeps it selected across the refill.
    agentchoices::setModels({ { QStringLiteral("Sonnet 5"), QStringLiteral("claude-sonnet-5") },
                               { QStringLiteral("Next"), QStringLiteral("claude-next-1") } });
    if (agentchoices::models().size() != 3) {
        // "Default" plus the two just given.
        agentchoices::resetModelsForTest();
        return 13;
    }
    agentchoices::fillModelCombo(&models, agentchoices::modelComboSelection(&models));
    if (models.count() != 3 ||
        models.currentData().toString() != QLatin1String("claude-sonnet-5")) {
        agentchoices::resetModelsForTest();
        return 14;
    }

    // An id the new list dropped -- here, one that was never on any list --
    // is kept as typed text rather than snapping to "Default".
    agentchoices::fillModelCombo(&typed, agentchoices::modelComboSelection(&typed));
    if (typed.currentText() != QLatin1String("claude-something-new")) {
        agentchoices::resetModelsForTest();
        return 15;
    }

    // Cleanup: every check after this one in the same process must see the
    // built-in fallback, not this check's simulated fetch.
    agentchoices::resetModelsForTest();
    if (agentchoices::models().size() != 5) {
        return 16;
    }
    return 0;
}

void agentchoices::resetModelsForTest() {
    backendModels.clear();
    backendModes.clear();
    backendNotes.clear();
    hasFetchedModels() = false;
}

#endif // BS_WIDGET_TESTS
