#include "RunPanel.h"
#include "CodeView.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/run_panel.cxxqt.h"
#include <QAction>
#include <QActionGroup>
#include <QClipboard>
#include <QComboBox>
#include <QDesktopServices>
#include <QFont>
#include <QFontDatabase>
#include <QFrame>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPalette>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QSizePolicy>
#include <QSpinBox>
#include <QStandardItemModel>
#include <QTextCursor>
#include <QUrl>
#include <QVBoxLayout>

namespace {

/// The daemon's four words for a run, spelled as `run_state_word` writes them.
const char* kStarting = "starting";
const char* kReady = "ready";
const char* kFailed = "failed";
const char* kStopped = "stopped";

/// How many lines of output the log keeps, matching `RunLog::CAP` in Rust. The
/// widget drops the same oldest lines the model does, so scrolling back in the
/// panel and re-reading `logText` show the same thing.
constexpr int kLogLines = 2000;

/// The combo's tooltip when the daemon had nothing to complain about. The
/// warnings, when there are any, are appended to it rather than replacing it.
const QString kConfigTip =
    QStringLiteral("Run configuration, from bondsymphonic.toml or detected");

/// The glyphs the state column uses: a run coming up, one serving, one that
/// ended badly, and nothing running.
const QString kGlyphStarting = QStringLiteral("◌");
const QString kGlyphReady = QStringLiteral("●");
const QString kGlyphFailed = QStringLiteral("✗");
const QString kGlyphIdle = QStringLiteral("·");

/// Paints `label`'s text in `accent`, lifted for a dark palette.
void tint(QLabel* label, const QColor& accent) {
    QPalette colors = label->palette();
    colors.setColor(QPalette::WindowText, theme::ink(accent, theme::isDark(label->palette())));
    label->setPalette(colors);
}

/// Restores `label` to the palette's ordinary text colour.
void untint(QLabel* label) {
    label->setPalette(QPalette());
}

/// Makes `button` the face of `action`: the click goes through the action, and
/// the button is greyed out exactly when the action is.
///
/// One direction only. The panel and the Run menu show the same action, and
/// the enablement rules live in `updateRow`, which sets them on the action; a
/// button that could also be disabled on its own would be a second opinion
/// about whether a run can start.
void bindButton(QPushButton* button, QAction* action) {
    QObject::connect(button, &QPushButton::clicked, action, &QAction::trigger);
    QObject::connect(action, &QAction::changed, button,
                     [button, action] { button->setEnabled(action->isEnabled()); });
    button->setEnabled(action->isEnabled());
}

} // namespace

void runpanel::appendConfigItems(QComboBox* combo, const QString& configsJson) {
    // Two callers, two shapes: the model answers a bare array, while the
    // controller carries the daemon's whole `DetectRunConfigsResult` because
    // the dialog needs its `network_allow` as well.
    const QJsonDocument doc = QJsonDocument::fromJson(configsJson.toUtf8());
    const QJsonArray configs =
        doc.isArray() ? doc.array() : doc.object().value("configs").toArray();
    // A plain QComboBox is backed by a QStandardItemModel; the cast is what
    // makes an individual row greyed out and unpickable. A model that is not
    // one leaves every row enabled rather than dropping the entries.
    auto* items = qobject_cast<QStandardItemModel*>(combo->model());
    for (const QJsonValue& value : configs) {
        const QJsonObject config = value.toObject();
        const QString name = config.value("name").toString();
        if (name.isEmpty()) {
            continue;
        }
        const int port = config.value("port").toInt();
        const bool guessed = config.value("port_guessed").toBool();
        const QString reason = config.value("disabled_reason").toString();
        // The guessed port goes in the label rather than only in a tooltip: it
        // is the one thing about a detected configuration a user has to check
        // before starting it.
        combo->addItem(guessed ? QStringLiteral("%1 (guessed :%2)").arg(name).arg(port) : name,
                       name);
        const int row = combo->count() - 1;
        if (!reason.isEmpty()) {
            combo->setItemData(row, reason, Qt::ToolTipRole);
            if (items != nullptr && items->item(row) != nullptr) {
                items->item(row)->setEnabled(false);
            }
            continue;
        }
        QString tip = config.value("command").toString();
        if (guessed) {
            // A guessed port is the one thing about a detected configuration
            // the user may have to argue with, and the Run panel's port box is
            // where they do it. Pinning it in the repository's own file is the
            // permanent answer; the box is the per-workspace one.
            tip += QStringLiteral("\nPort %1 was guessed; set another in the Run panel, or pin it "
                                  "in bondsymphonic.toml.")
                       .arg(port);
        }
        combo->setItemData(row, tip, Qt::ToolTipRole);
    }
}

RunPanel::RunPanel(RunPanelModel* model, AppController* controller, QWidget* parent)
    : QWidget(parent), m_model(model), m_controller(controller) {
    auto* outer = new QVBoxLayout(this);
    outer->setContentsMargins(6, 4, 6, 4);
    outer->setSpacing(4);
    // Before the widgets: every button below is the face of one of these.
    buildActions();
    buildTopRow(outer);
    buildToast(outer);
    buildLog(outer);
    connectModel();
    rebuildConfigs();
}

void RunPanel::buildActions() {
    m_runAction = new QAction(QStringLiteral("&Run"), this);
    m_runAction->setObjectName(QStringLiteral("RunStartAction"));
    m_runAction->setToolTip(QStringLiteral("Start the selected run configuration"));
    QObject::connect(m_runAction, &QAction::triggered, this, &RunPanel::startRun);

    m_stopAction = new QAction(QStringLiteral("&Stop"), this);
    m_stopAction->setObjectName(QStringLiteral("RunStopAction"));
    QObject::connect(m_stopAction, &QAction::triggered, this, [this] {
        if (!m_model.isNull()) {
            m_model->stop();
        }
    });

    m_restartAction = new QAction(QStringLiteral("Res&tart run"), this);
    m_restartAction->setObjectName(QStringLiteral("RunRestartAction"));
    QObject::connect(m_restartAction, &QAction::triggered, this, &RunPanel::onRestart);

    m_openAction = new QAction(QStringLiteral("&Open in browser"), this);
    m_openAction->setObjectName(QStringLiteral("RunOpenAction"));
    QObject::connect(m_openAction, &QAction::triggered, this, &RunPanel::openUrl);

    // The widget's own copy, not the model's ring buffer: a user clearing the
    // output wants the screen empty, and the lines a finished run left behind
    // are still what `logText` answers with if anything asks for them again.
    m_clearOutputAction = new QAction(QStringLiteral("C&lear output"), this);
    m_clearOutputAction->setObjectName(QStringLiteral("RunClearOutputAction"));
    QObject::connect(m_clearOutputAction, &QAction::triggered, this, [this] { m_log->clear(); });

    m_copyOutputAction = new QAction(QStringLiteral("&Copy output"), this);
    m_copyOutputAction->setObjectName(QStringLiteral("RunCopyOutputAction"));
    QObject::connect(m_copyOutputAction, &QAction::triggered, this, [this] {
        QGuiApplication::clipboard()->setText(m_log->toPlainText());
    });

    m_allowHostAction = new QAction(QStringLiteral("&Allow blocked host…"), this);
    m_allowHostAction->setObjectName(QStringLiteral("RunAllowHostAction"));
    // Dead until there is a host to allow. The toast raises it with the offer
    // and `setToastBusy` is the one place that decides.
    m_allowHostAction->setEnabled(false);
    QObject::connect(m_allowHostAction, &QAction::triggered, this, &RunPanel::onAllowClicked);

    m_configActions = new QActionGroup(this);
    m_configActions->setExclusive(true);
}

void RunPanel::buildTopRow(QVBoxLayout* outer) {
    auto* row = new QHBoxLayout();
    row->setSpacing(6);

    m_configs = new QComboBox(this);
    // Named so a test or a screenshot harness can find the four widgets that
    // carry the panel's state without guessing at child order.
    m_configs->setObjectName("RunConfigCombo");
    m_configs->setSizeAdjustPolicy(QComboBox::AdjustToContents);
    m_configs->setMinimumContentsLength(16);
    m_configs->setToolTip(kConfigTip);
    row->addWidget(m_configs, 0);

    m_port = new QSpinBox(this);
    m_port->setObjectName("RunPortSpin");
    // 0 is "no override" rather than a port, and says so instead of showing a
    // number the daemon would refuse.
    m_port->setRange(0, 65535);
    m_port->setSpecialValueText("auto");
    m_port->setPrefix(":");
    row->addWidget(m_port, 0);

    m_start = new QPushButton("Start", this);
    m_start->setObjectName("RunStartButton");
    m_stop = new QPushButton("Stop", this);
    m_stop->setObjectName("RunStopButton");
    row->addWidget(m_start, 0);
    row->addWidget(m_stop, 0);

    m_glyph = new QLabel(kGlyphIdle, this);
    m_glyph->setFixedWidth(m_glyph->fontMetrics().horizontalAdvance(kGlyphFailed) * 2);
    m_glyph->setAlignment(Qt::AlignCenter);
    row->addWidget(m_glyph, 0);

    // Rich text so the address is a link as well as a label. The href is never
    // followed by Qt itself: `openUrl` is the single place the IDE hands a URL
    // to the system browser.
    m_url = new QLabel(this);
    m_url->setTextFormat(Qt::RichText);
    m_url->setOpenExternalLinks(false);
    m_url->setTextInteractionFlags(Qt::TextBrowserInteraction);
    m_url->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    row->addWidget(m_url, 1);

    m_open = new QPushButton("Open", this);
    m_open->setToolTip("Open the running application in the system browser");
    row->addWidget(m_open, 0);
    outer->addLayout(row);

    // Above the run's own status line and separate from it: a complaint about
    // `bondsymphonic.toml` is a fact about the repository's file, not news
    // about a run, and the two must not take turns in one label. Word-wrapped,
    // because a single complaint is shown as the daemon wrote it.
    m_warnings = new QLabel(this);
    m_warnings->setObjectName("RunWarningsLabel");
    m_warnings->setTextFormat(Qt::PlainText);
    m_warnings->setWordWrap(true);
    m_warnings->setVisible(false);
    outer->addWidget(m_warnings);

    m_status = new QLabel(this);
    m_status->setObjectName("RunStatusLabel");
    m_status->setTextFormat(Qt::PlainText);
    m_status->setWordWrap(true);
    m_status->setVisible(false);
    outer->addWidget(m_status);

    QObject::connect(m_configs, &QComboBox::activated, this, &RunPanel::onConfigActivated);
    QObject::connect(m_port, &QSpinBox::valueChanged, this, &RunPanel::onPortChanged);
    bindButton(m_start, m_runAction);
    bindButton(m_stop, m_stopAction);
    bindButton(m_open, m_openAction);
    QObject::connect(m_url, &QLabel::linkActivated, this, [this](const QString&) { openUrl(); });
}

void RunPanel::buildToast(QVBoxLayout* outer) {
    m_toast = new QFrame(this);
    m_toast->setObjectName("RunDenialToast");
    m_toast->setFrameShape(QFrame::StyledPanel);
    m_toast->setAutoFillBackground(true);
    QPalette toastPalette = m_toast->palette();
    // Red: the sandbox refused a connection. It is a report of something
    // blocked, not a question about something waiting, so it is not the amber
    // the permission bar uses.
    toastPalette.setColor(QPalette::Window,
                          codeview::wash(palette().base().color(), theme::removed(),
                                         codeview::kWashAmount));
    m_toast->setPalette(toastPalette);

    auto* layout = new QHBoxLayout(m_toast);
    layout->setContentsMargins(6, 4, 6, 4);
    layout->setSpacing(8);
    m_toastText = new QLabel(m_toast);
    m_toastText->setTextFormat(Qt::PlainText);
    m_toastText->setWordWrap(true);
    m_toastText->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    layout->addWidget(m_toastText, 1);

    m_allow = new QPushButton("Allow host", m_toast);
    m_allow->setToolTip("Add the host to this workspace's allowlist");
    m_dismiss = new QPushButton("Dismiss", m_toast);
    layout->addWidget(m_allow, 0);
    layout->addWidget(m_dismiss, 0);
    m_toast->hide();
    outer->addWidget(m_toast);

    bindButton(m_allow, m_allowHostAction);
    QObject::connect(m_dismiss, &QPushButton::clicked, this, &RunPanel::onDismissClicked);
}

void RunPanel::buildLog(QVBoxLayout* outer) {
    m_log = new QPlainTextEdit(this);
    m_log->setObjectName("RunLog");
    m_log->setReadOnly(true);
    m_log->setMaximumBlockCount(kLogLines);
    // Long lines scroll rather than wrap: a stack trace or a bundler's progress
    // line reads as one line, and a wrapped one hides how many there were.
    m_log->setLineWrapMode(QPlainTextEdit::NoWrap);
    QFont font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    font.setStyleHint(QFont::Monospace);
    font.setFixedPitch(true);
    m_log->setFont(font);
    m_log->setPlaceholderText("No output yet.");
    outer->addWidget(m_log, 1);
}

void RunPanel::connectModel() {
    if (m_model.isNull()) {
        return;
    }
    RunPanelModel* model = m_model;
    // `this` as the context object throughout: the model is parented to the
    // application and outlives the window, so a connection without one would
    // reach into a deleted panel.
    QObject::connect(model, &RunPanelModel::configsChanged, this, &RunPanel::rebuildConfigs);
    QObject::connect(model, &RunPanelModel::selectedConfigChanged, this, &RunPanel::syncSelection);
    QObject::connect(model, &RunPanelModel::stateChanged, this, &RunPanel::updateRow);
    QObject::connect(model, &RunPanelModel::runsChanged, this, &RunPanel::updateRow);
    // Only the row: the toast's buttons are armed by the answer to its own
    // `allowHost`, not by the model-wide busy flag. An unrelated `run.start`
    // finishing mid-allow would otherwise re-arm the offer and let a second
    // click send a duplicate `set_allowlist`.
    QObject::connect(model, &RunPanelModel::busyChanged, this, &RunPanel::updateRow);
    QObject::connect(model, &RunPanelModel::denialFailed, this, &RunPanel::onDenialFailed);
    QObject::connect(model, &RunPanelModel::outputAppended, this, &RunPanel::onOutputAppended);
    // The queue, and which host is being offered from it, are the model's. The
    // panel raises exactly what it is told to and takes it down when told,
    // including on a switch to a workspace with nothing waiting.
    QObject::connect(model, &RunPanelModel::denied, this, &RunPanel::showToast);
    QObject::connect(model, &RunPanelModel::deniedCleared, this, &RunPanel::hideToast);
    QObject::connect(model, &RunPanelModel::errorOccurred, this, [this](const QString& message) {
        m_lastError = message;
        // A stop that failed is a restart that will not happen. Leaving the
        // flag set would arm a run at whatever unrelated moment the panel next
        // found itself idle.
        m_restartPending = false;
        updateRow();
    });
    // A different workspace has a different log and its own last failure.
    QObject::connect(model, &RunPanelModel::workspaceIdChanged, this, [this] {
        // The override is per workspace and per configuration, so the box has
        // to be re-read rather than carried across.
        syncPort();
        m_lastError.clear();
        // The log on screen belongs to a run of the workspace that has gone,
        // and the new one may have no run at all -- in which case the run id
        // does not change and only this flag would tell the widget to empty.
        m_logStale = true;
        updateRow();
    });
}

void RunPanel::startRun() {
    if (m_model.isNull()) {
        return;
    }
    // A new run's first news replaces the last one's obituary.
    m_lastError.clear();
    m_model->start();
    updateRow();
}

void RunPanel::onRestart() {
    if (m_model.isNull()) {
        return;
    }
    if (m_model->getActiveState().isEmpty()) {
        // Nothing to replace: a restart of a run that has already ended is a
        // run, and refusing it would be pedantry.
        startRun();
        return;
    }
    // The model refuses a second run of a configuration that is still alive,
    // so the start waits for the daemon's own `stopped` to come back through
    // `updateRow`.
    m_restartPending = true;
    m_model->stop();
}

void RunPanel::rebuildConfigs() {
    if (m_model.isNull()) {
        return;
    }
    m_configs->clear();
    runpanel::appendConfigItems(m_configs, m_model->configsJson());
    updateWarnings();
    syncSelection();
    rebuildConfigActions();
}

void RunPanel::rebuildConfigActions() {
    // Deleted rather than reused: the list is the daemon's, and a run
    // configuration that has gone from `bondsymphonic.toml` must leave the
    // menu with it. Whatever is showing them is told to ask again.
    for (QAction* action : m_configActions->actions()) {
        m_configActions->removeAction(action);
        delete action;
    }
    for (int index = 0; index < m_configs->count(); ++index) {
        const QString name = m_configs->itemData(index).toString();
        auto* action = new QAction(m_configs->itemText(index), m_configActions);
        action->setCheckable(true);
        action->setData(name);
        action->setToolTip(m_configs->itemData(index, Qt::ToolTipRole).toString());
        // A configuration the daemon disabled is shown and unpickable in the
        // menu exactly as it is in the combo: visible without being offerable
        // is the whole point of listing it.
        action->setEnabled(m_configs->model()->index(index, 0).flags().testFlag(Qt::ItemIsEnabled));
        QObject::connect(action, &QAction::triggered, this, [this, name] { selectConfig(name); });
    }
    syncConfigActionChecks();
    Q_EMIT configurationsChanged();
}

void RunPanel::syncConfigActionChecks() {
    const QString selected = m_model.isNull() ? QString() : m_model->getSelectedConfig();
    for (QAction* action : m_configActions->actions()) {
        action->setChecked(action->data().toString() == selected);
    }
}

void RunPanel::selectConfig(const QString& name) {
    if (m_model.isNull()) {
        return;
    }
    m_model->selectConfig(name);
    // The model refuses an unknown or disabled name in silence, so what it
    // settled on is read back rather than assumed.
    syncSelection();
}

void RunPanel::updateWarnings() {
    if (m_model.isNull()) {
        return;
    }
    // The daemon loads the `[[run]]` entries it can parse and reports what it
    // could not, instead of discarding the whole file. An entry that is not in
    // the combo and was not complained about leaves the user staring at a file
    // they believe is correct, so both halves of the answer are shown: every
    // complaint on the combo the entries are missing from -- a parse error runs
    // to several lines and a tooltip is where those fit -- and one line under
    // the row.
    const QString detail = m_model->configWarnings();
    m_configs->setToolTip(detail.isEmpty() ? kConfigTip
                                           : kConfigTip + QStringLiteral("\n\n") + detail);
    const QString status = m_model->configWarningStatus();
    m_warnings->setText(status);
    m_warnings->setToolTip(detail);
    m_warnings->setVisible(!status.isEmpty());
    if (!status.isEmpty()) {
        tint(m_warnings, theme::renamed());
    } else {
        untint(m_warnings);
    }
}

void RunPanel::syncSelection() {
    if (m_model.isNull()) {
        return;
    }
    const QString selected = m_model->getSelectedConfig();
    // `findData` on an empty name finds nothing, which is the index -1 an empty
    // selection wants anyway.
    m_configs->setCurrentIndex(selected.isEmpty() ? -1 : m_configs->findData(selected));
    syncConfigActionChecks();
    syncPort();
    updateRow();
}

void RunPanel::syncPort() {
    if (m_model.isNull()) {
        return;
    }
    const QString workspaceId = m_model->getWorkspaceId();
    const QString config = m_model->getSelectedConfig();
    const QJsonObject selected =
        QJsonDocument::fromJson(m_model->selectedConfigJson().toUtf8()).object();
    // The daemon's own words: `port_guessed` is true when it inferred the port
    // from the command rather than reading it out of `bondsymphonic.toml`. Only
    // a guess is the user's to argue with.
    const bool guessed = selected.value("port_guessed").toBool();
    const int configured = selected.value("port").toInt();
    const int override =
        m_controller.isNull() || workspaceId.isEmpty() || config.isEmpty()
            ? 0
            : m_controller->portOverride(workspaceId, config);

    m_syncingPort = true;
    // The override if there is one, else the configuration's own port as the
    // number the run will actually use. Editing from that number is what makes
    // the field an adjustment rather than a blank to fill in.
    m_port->setValue(override != 0 ? override : configured);
    m_syncingPort = false;

    m_port->setEnabled(guessed && !workspaceId.isEmpty());
    m_port->setVisible(!config.isEmpty());
    // The box shows the port the next run will actually use -- the override if
    // there is one, else the configuration's own -- rather than starting blank,
    // so it reads as an adjustment to a real number. Spinning down to `auto`
    // (or typing the configuration's own port back) is what clears the
    // override and hands the choice back to the guess.
    m_port->setToolTip(
        guessed ? QStringLiteral("The port the next run will use. The daemon guessed %1; change "
                                 "this to run on another one, or set it to `auto` to go back to "
                                 "the guess.")
                      .arg(configured)
                : QStringLiteral("This port comes from the repository's bondsymphonic.toml and is "
                                 "not overridden here."));
}

void RunPanel::onPortChanged(int port) {
    if (m_syncingPort || m_model.isNull() || m_controller.isNull()) {
        return;
    }
    const QString workspaceId = m_model->getWorkspaceId();
    const QString config = m_model->getSelectedConfig();
    if (workspaceId.isEmpty() || config.isEmpty()) {
        return;
    }
    const QJsonObject selected =
        QJsonDocument::fromJson(m_model->selectedConfigJson().toUtf8()).object();
    // Back at the configuration's own port is not an override: recording one
    // would pin a number that the repository is entitled to change.
    const int configured = selected.value("port").toInt();
    m_controller->setPortOverride(workspaceId, config, port == configured ? 0 : port);
}

void RunPanel::onConfigActivated(int index) {
    selectConfig(m_configs->itemData(index).toString());
}

void RunPanel::showLogOf(const QString& runId) {
    if (runId == m_shownRunId && !m_logStale) {
        return;
    }
    m_logStale = false;
    m_shownRunId = runId;
    // The whole log at once: the lines that arrived while another tab was in
    // front never reached this widget as `outputAppended`.
    m_log->setPlainText(runId.isEmpty() ? QString() : m_model->logText(runId));
    m_log->moveCursor(QTextCursor::End);
}

void RunPanel::onOutputAppended(const QString& runId, const QString& line) {
    if (runId == m_shownRunId && !runId.isEmpty()) {
        m_log->appendPlainText(line);
    }
}

QJsonObject RunPanel::lastRunOfSelected() const {
    const QString selected = m_model->getSelectedConfig();
    QJsonObject last;
    if (selected.isEmpty()) {
        return last;
    }
    // The daemon's own order, so the last entry for the configuration is its
    // most recent run. Which of them is *alive* is `activeState`'s business;
    // this is only what the row says when none of them is.
    const QJsonArray runs = QJsonDocument::fromJson(m_model->runsJson().toUtf8()).array();
    for (const QJsonValue& value : runs) {
        const QJsonObject run = value.toObject();
        if (run.value("config_name").toString() == selected) {
            last = run;
        }
    }
    return last;
}

void RunPanel::updateRow() {
    if (m_model.isNull()) {
        return;
    }
    const QString state = m_model->getActiveState();
    const QString url = m_model->getActiveUrl();
    const bool ready = state == QLatin1String(kReady);
    const bool starting = state == QLatin1String(kStarting);

    if (m_restartPending && state.isEmpty() && !m_model->getBusy()) {
        // The stop a restart asked for has landed. Cleared first: `startRun`
        // comes back through here, and a flag still set would start a second
        // run of the one it has just started.
        m_restartPending = false;
        startRun();
        return;
    }

    const bool canStart =
        !m_model->getSelectedConfig().isEmpty() && state.isEmpty() && !m_model->getBusy();
    m_runAction->setEnabled(canStart);
    m_stopAction->setEnabled(ready || starting);
    m_openAction->setEnabled(ready && !url.isEmpty());
    // Restart is a stop and a start, so it is offered whenever either half
    // would be: on a live run it replaces it, and on a finished one it is a
    // second way to say Run.
    m_restartAction->setEnabled(canStart || ready || starting);

    if (ready && !url.isEmpty()) {
        m_url->setText(QStringLiteral("<a href=\"%1\">%1</a>").arg(url.toHtmlEscaped()));
        m_url->setToolTip(url);
    } else if (starting) {
        m_url->setText("starting…");
        m_url->setToolTip(QString());
    } else {
        m_url->clear();
        m_url->setToolTip(QString());
    }

    // A finished run leaves `activeState` empty, so the glyph, the status line
    // and the log all fall back to the last run of this configuration. Its
    // output is exactly what a user wants to read once a run has stopped or
    // failed, so it stays on screen until another run replaces it.
    const QJsonObject last = state.isEmpty() ? lastRunOfSelected() : QJsonObject();
    showLogOf(state.isEmpty() ? last.value("run_id").toString() : m_model->getActiveRunId());
    const QString ended = last.value("state").toString();
    const bool failed = ended == QLatin1String(kFailed);
    if (starting) {
        m_glyph->setText(kGlyphStarting);
        tint(m_glyph, theme::changed());
    } else if (ready) {
        m_glyph->setText(kGlyphReady);
        tint(m_glyph, theme::added());
    } else if (failed) {
        m_glyph->setText(kGlyphFailed);
        tint(m_glyph, theme::removed());
    } else {
        m_glyph->setText(kGlyphIdle);
        untint(m_glyph);
    }
    m_glyph->setToolTip(state.isEmpty() ? ended : state);

    // A failed request outranks a finished run: it is the newer news, and it is
    // the one the user can do something about.
    if (!m_lastError.isEmpty()) {
        m_status->setText(m_lastError);
        tint(m_status, theme::removed());
        m_status->setVisible(true);
        return;
    }
    const QString name = m_model->getSelectedConfig();
    const QString detail = last.value("detail").toString();
    if (failed) {
        m_status->setText(detail.isEmpty() ? QStringLiteral("%1 failed.").arg(name)
                                           : QStringLiteral("%1 failed: %2").arg(name, detail));
        tint(m_status, theme::removed());
        m_status->setVisible(true);
        return;
    }
    if (ended == QLatin1String(kStopped)) {
        m_status->setText(QStringLiteral("%1 stopped.").arg(name));
        untint(m_status);
        m_status->setVisible(true);
        return;
    }
    m_status->clear();
    m_status->setVisible(false);
}

void RunPanel::showToast(const QString& host) {
    if (host.isEmpty()) {
        return;
    }
    m_deniedHost = host;
    // A new offer starts clean: the previous host's failure is not this one's.
    m_denialError.clear();
    setToastBusy(false);
    m_toast->show();
}

void RunPanel::hideToast() {
    m_deniedHost.clear();
    m_denialError.clear();
    setToastBusy(false);
    m_toastText->clear();
    m_toast->hide();
}

void RunPanel::setToastBusy(bool busy) {
    // The action rather than the button, which follows it: the Run menu offers
    // the same action, and there it must be dead whenever there is no host
    // waiting rather than offering to allow nothing.
    m_allowHostAction->setEnabled(!busy && !m_deniedHost.isEmpty());
    m_dismiss->setEnabled(!busy);
    if (m_deniedHost.isEmpty()) {
        return;
    }
    // The wording is part of the state: a toast whose buttons came back after a
    // failed allow must stop claiming the host is being allowed, and must say
    // what went wrong where the offer it belongs to is.
    if (busy) {
        m_toastText->setText(QStringLiteral("Allowing %1…").arg(m_deniedHost));
        return;
    }
    const QString blocked = QStringLiteral("Blocked network access to %1").arg(m_deniedHost);
    m_toastText->setText(m_denialError.isEmpty()
                             ? blocked
                             : QStringLiteral("%1 — %2").arg(blocked, m_denialError));
}

void RunPanel::onDenialFailed(const QString& host, const QString& message) {
    // A denial answered while another workspace's toast is up: the model has
    // already moved on, and re-arming this one would arm the wrong offer.
    if (host != m_deniedHost) {
        return;
    }
    m_denialError = message;
    setToastBusy(false);
}

void RunPanel::onAllowClicked() {
    const QString host = m_deniedHost;
    if (host.isEmpty() || m_model.isNull()) {
        return;
    }
    m_denialError.clear();
    // The toast stays up until the model says the host really is allowed: the
    // daemon has to be asked what its allowlist is and then told the new one,
    // and a call that fails must not look like one that worked. Only the
    // buttons go, so the offer cannot be answered twice while the answer is
    // out.
    setToastBusy(true);
    m_lastError.clear();
    updateRow();
    m_model->allowHost(host);
}

void RunPanel::onDismissClicked() {
    const QString host = m_deniedHost;
    if (host.isEmpty() || m_model.isNull()) {
        return;
    }
    // The model clears the head and republishes at once, so the toast is taken
    // down -- or replaced by the next host -- by `deniedCleared`/`denied`
    // before this returns.
    m_model->dismissDenied(host);
}

QAction* RunPanel::runAction() const { return m_runAction; }

QAction* RunPanel::stopAction() const { return m_stopAction; }

QAction* RunPanel::restartAction() const { return m_restartAction; }

QAction* RunPanel::openAction() const { return m_openAction; }

QAction* RunPanel::clearOutputAction() const { return m_clearOutputAction; }

QAction* RunPanel::copyOutputAction() const { return m_copyOutputAction; }

QAction* RunPanel::allowHostAction() const { return m_allowHostAction; }

QList<QAction*> RunPanel::configurationActions() const { return m_configActions->actions(); }

#if defined(BS_WIDGET_TESTS)
void RunPanel::noteConfigsForTest(const QString& configsJson) {
    m_configs->clear();
    runpanel::appendConfigItems(m_configs, configsJson);
    rebuildConfigActions();
}
#endif

void RunPanel::openUrl() {
    if (m_model.isNull()) {
        return;
    }
    const QString url = m_model->getActiveUrl();
    if (!url.isEmpty()) {
        QDesktopServices::openUrl(QUrl(url));
    }
}
