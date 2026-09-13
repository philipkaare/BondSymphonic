#include "NewAgentDialog.h"
#include "RunPanel.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QAbstractItemModel>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QDir>
#include <QFileDialog>
#include <QFontMetrics>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QStringList>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// The entry that turns the group combo into a free-text field.
const char* kNewGroupEntry = "New group…";

/// The adapters as the daemon spells them.
const char* kClaudeAdapter = "claude";
const char* kTerminalAdapter = "terminal";

/// How many lines of opening prompt are visible before the box scrolls.
constexpr int kPromptRows = 4;

/// The run-config entry that means "record nothing on the tab". First in the
/// list and selected until the daemon has answered, so a dialog accepted before
/// the detection lands creates a workspace with no run configuration rather
/// than with a guess.
const char* kNoRunConfig = "(none)";

/// Whether the daemon has *said* it cannot run Claude Code.
///
/// Read this way round on purpose. The capabilities arrive with the `hello`
/// reply, and this dialog can be built before that lands -- the window opens
/// it the moment the user asks -- so "the list does not mention Claude" is not
/// the same as "there is no Claude". Treating the two alike is what created
/// terminal workspaces for users who asked for an agent and got a shell: every
/// daemon this IDE talks to offers both adapters, and the only thing missing
/// was the answer that says so. A daemon that really does report a list
/// without Claude in it is still believed.
bool daemonLacksClaude(AppController* controller) {
    const QJsonObject capabilities =
        QJsonDocument::fromJson(controller->capabilitiesJson().toUtf8()).object();
    if (!capabilities.contains(QStringLiteral("adapters"))) {
        return false;
    }
    const QJsonArray adapters = capabilities.value(QStringLiteral("adapters")).toArray();
    return !adapters.contains(QJsonValue(QString::fromUtf8(kClaudeAdapter)));
}

/// The models to offer, newest and most capable first, as label and `--model`
/// argument. The list is a convenience and not a limit: the combo is editable,
/// the daemon passes whatever it is given straight to the CLI, and an empty
/// choice leaves `--model` off altogether so Claude Code's own default wins.
struct ModelChoice {
    const char* label;
    const char* id;
};
const ModelChoice kModels[] = {
    { "Default (Claude Code decides)", "" },
    { "Opus 5", "claude-opus-5" },
    { "Sonnet 5", "claude-sonnet-5" },
    { "Haiku 4.5", "claude-haiku-4-5-20251001" },
};

} // namespace

QString NewAgentDialog::initialRepoPath(AppController* controller) {
    const QJsonArray paths = QJsonDocument::fromJson(controller->recentRepos().toUtf8()).array();
    // The most recent, which is the one a second agent on the same project
    // wants and the one a first run has none of.
    return paths.isEmpty() ? QString() : paths.first().toString();
}

NewAgentDialog::NewAgentDialog(AppController* controller, GroupModel* model,
                               const QString& initialPath, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_model(model) {
    setWindowTitle("New Agent");
    setModal(true);

    auto* outer = new QVBoxLayout(this);
    m_form = new QFormLayout();
    QFormLayout* form = m_form;
    outer->addLayout(form);

    auto* repoRow = new QHBoxLayout();
    m_repoPath = new QLineEdit(this);
    m_repoPath->setObjectName("NewAgentRepoPath");
    m_repoPath->setPlaceholderText("C:\\git\\project or /home/you/project");
    m_recent = new QToolButton(this);
    m_recent->setObjectName("NewAgentRecentButton");
    m_recent->setText("Recent");
    m_recent->setToolTip("Repositories you have created workspaces in before");
    m_recent->setPopupMode(QToolButton::InstantPopup);
    m_recent->setMenu(new QMenu(m_recent));
    auto* browseButton = new QPushButton("Browse…", this);
    repoRow->addWidget(m_repoPath, 1);
    repoRow->addWidget(m_recent, 0);
    repoRow->addWidget(browseButton, 0);
    form->addRow("Repository:", repoRow);
    buildRecentMenu();

    m_repoState = new QLabel(this);
    m_repoState->setWordWrap(true);
    // Plain text: the sentence is the dialog's, but the path in it is the
    // user's, and a path is not markup.
    m_repoState->setTextFormat(Qt::PlainText);
    m_repoState->hide();
    form->addRow(QString(), m_repoState);

    m_baseBranch = new QComboBox(this);
    m_baseBranch->setEditable(true);
    form->addRow("Base branch:", m_baseBranch);

    int tabs = 0;
    for (int g = 0; g < m_model->groupCount(); ++g) {
        tabs += m_model->tabCount(g);
    }
    m_name = new QLineEdit(QString("agent-%1").arg(tabs + 1), this);
    form->addRow("Name:", m_name);

    m_nameHint = new QLabel(this);
    m_nameHint->setObjectName(QStringLiteral("NewAgentNameHint"));
    m_nameHint->setWordWrap(true);
    m_nameHint->setTextFormat(Qt::PlainText);
    // The one colour in this dialog that means "this will not work". The IDE's
    // red, lifted for a dark palette the way every other accent is, rather than
    // a hex of its own: `theme` has no "error", and a name that cannot be used
    // is the same judgement as a line that is gone.
    m_nameHint->setStyleSheet(
        QStringLiteral("color:%1")
            .arg(theme::ink(theme::removed(), theme::isDark(palette())).name()));
    m_nameHint->hide();
    form->addRow(QString(), m_nameHint);

    m_adapter = new QComboBox(this);
    // Claude Code first, so it is the default: it is what the IDE is for, and
    // a terminal workspace is the fallback rather than the usual case.
    m_adapter->addItem("Claude Code", kClaudeAdapter);
    m_adapter->addItem("Terminal", kTerminalAdapter);
    m_adapter->setCurrentIndex(daemonLacksClaude(m_controller) ? 1 : 0);
    form->addRow("Adapter:", m_adapter);

    m_command = new QLineEdit(this);
    m_command->setPlaceholderText("default shell");
    form->addRow("Command:", m_command);

    m_claudeModel = new QComboBox(this);
    m_claudeModel->setObjectName(QStringLiteral("NewAgentModel"));
    // Editable, so a model that is not on the list is still one keystroke
    // away; `optionsJson` reads the text rather than the selection because of
    // it.
    m_claudeModel->setEditable(true);
    m_claudeModel->setInsertPolicy(QComboBox::NoInsert);
    m_claudeModel->setToolTip("Which model the agent runs. Anything Claude Code accepts can be "
                              "typed here as well.");
    for (const ModelChoice& choice : kModels) {
        m_claudeModel->addItem(QString::fromUtf8(choice.label), QString::fromUtf8(choice.id));
    }
    m_claudeModel->setCurrentIndex(0);
    form->addRow("Model:", m_claudeModel);

    m_permissionMode = new QComboBox(this);
    // The words the daemon validates `--permission-mode` against; it rejects
    // anything else, so they are spelled exactly as the CLI spells them.
    for (const char* mode : { "default", "acceptEdits", "plan", "dontAsk" }) {
        m_permissionMode->addItem(QString::fromUtf8(mode), QString::fromUtf8(mode));
    }
    m_permissionMode->setCurrentIndex(
        qMax(0, m_permissionMode->findData(m_controller->defaultPermissionMode())));
    form->addRow("Permission mode:", m_permissionMode);

    m_initialPrompt = new QPlainTextEdit(this);
    m_initialPrompt->setPlaceholderText("What should the agent start on?");
    const QFontMetrics promptMetrics(m_initialPrompt->font());
    m_initialPrompt->setFixedHeight(kPromptRows * promptMetrics.lineSpacing() +
                                    2 * static_cast<int>(m_initialPrompt->frameWidth()) +
                                    2 * static_cast<int>(m_initialPrompt->document()->documentMargin()));
    form->addRow("Initial prompt:", m_initialPrompt);

    m_runConfig = new QComboBox(this);
    m_runConfig->addItem(kNoRunConfig, QString());
    m_runConfig->setToolTip("How the Run panel starts this workspace's application");
    form->addRow("Run config:", m_runConfig);

    m_networkNote = new QLabel(this);
    m_networkNote->setWordWrap(true);
    // The repository wrote these host names, so they are shown as text and
    // never as markup.
    m_networkNote->setTextFormat(Qt::PlainText);
    m_networkNote->hide();
    form->addRow(QString(), m_networkNote);

    m_group = new QComboBox(this);
    for (int g = 0; g < m_model->groupCount(); ++g) {
        m_group->addItem(m_model->groupName(g));
    }
    m_group->addItem(kNewGroupEntry);
    form->addRow("Group:", m_group);

    m_newGroup = new QLineEdit(this);
    m_newGroupLabel = new QLabel("New group name:", this);
    form->addRow(m_newGroupLabel, m_newGroup);
    m_newGroupLabel->setVisible(false);
    m_newGroup->setVisible(false);

    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("NewAgentStatus"));
    // What lands here is written by the daemon -- a repository path, a git
    // error, a name rule's complaint. `QLabel` guesses at a format otherwise,
    // and a message carrying angle brackets or an ampersand would be read as
    // markup and come out mangled or half-swallowed.
    m_status->setTextFormat(Qt::PlainText);
    m_status->setWordWrap(true);
    outer->addWidget(m_status);

    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText("Create");
    outer->addWidget(m_buttons);

    QObject::connect(m_name, &QLineEdit::textChanged, this, &NewAgentDialog::updateNameHint);
    QObject::connect(browseButton, &QPushButton::clicked, this, &NewAgentDialog::browse);
    QObject::connect(m_repoPath, &QLineEdit::editingFinished, this, &NewAgentDialog::inspectRepo);
    QObject::connect(m_repoPath, &QLineEdit::textChanged, this, &NewAgentDialog::updateOkEnabled);
    QObject::connect(m_baseBranch, &QComboBox::currentTextChanged, this, &NewAgentDialog::updateOkEnabled);
    QObject::connect(m_newGroup, &QLineEdit::textChanged, this, &NewAgentDialog::updateOkEnabled);
    QObject::connect(m_group, &QComboBox::currentIndexChanged, this, &NewAgentDialog::onGroupChanged);
    QObject::connect(m_adapter, &QComboBox::currentIndexChanged, this, &NewAgentDialog::onAdapterChanged);
    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    QObject::connect(m_controller, &AppController::repoInspected, this, &NewAgentDialog::onRepoInspected);
    QObject::connect(m_controller, &AppController::runConfigsDetected, this,
                     &NewAgentDialog::onRunConfigsDetected);
    QObject::connect(m_controller, &AppController::repoInspectFailed, this,
                     &NewAgentDialog::onRepoInspectFailed);
    // The catch-all, for the one lookup this dialog makes that has no signal of
    // its own. Everything else on it belongs to the window.
    QObject::connect(m_controller, &AppController::operationFailed, this,
                     &NewAgentDialog::onRunConfigsFailed);

    onAdapterChanged();
    updateNameHint();
    if (!initialPath.isEmpty()) {
        m_repoPath->setText(initialPath);
        // Asked for here, by the dialog that shows the answer. The window used
        // to inspect the path first and open this dialog on the reply, which
        // meant a repository on a slow mount left the user in front of a window
        // that had gone quiet, with a wait cursor and a status line borrowed
        // from whatever it said last. The dialog appears at once instead, says
        // what it is doing in its own status line, and has a Cancel button --
        // and Create stays dead until the branches are in.
        inspectRepo();
    }
    updateOkEnabled();
}

void NewAgentDialog::setGroup(const QString& name) {
    if (name.isEmpty()) {
        return;
    }
    const int index = m_group->findText(name);
    if (index >= 0) {
        m_group->setCurrentIndex(index);
    }
}

QString NewAgentDialog::repoPath() const {
    return m_controller->wslPath(m_repoPath->text().trimmed());
}

QString NewAgentDialog::baseBranch() const { return m_baseBranch->currentText().trimmed(); }

QString NewAgentDialog::name() const { return m_name->text().trimmed(); }

QString NewAgentDialog::adapter() const { return m_adapter->currentData().toString(); }

QString NewAgentDialog::command() const {
    // A Claude workspace runs no command of its own; the adapter is the
    // command, and sending a stale line edit would be a lie about the tab.
    return adapter() == QString::fromUtf8(kTerminalAdapter) ? m_command->text().trimmed() : QString();
}

QString NewAgentDialog::optionsJson() const {
    if (adapter() != QString::fromUtf8(kClaudeAdapter)) {
        return QString();
    }
    QJsonObject options;
    const QString model = chosenModel();
    if (!model.isEmpty()) {
        // Left out rather than sent empty: the daemon's own default is what an
        // untouched field means, and "" is not a model name.
        options.insert("model", model);
    }
    const QString mode = m_permissionMode->currentData().toString();
    if (!mode.isEmpty()) {
        options.insert("permission_mode", mode);
    }
    return QString::fromUtf8(QJsonDocument(options).toJson(QJsonDocument::Compact));
}

QString NewAgentDialog::chosenModel() const {
    const QString shown = m_claudeModel->currentText().trimmed();
    // A label off the list stands for the id behind it -- nobody types
    // "Opus 5" at a CLI -- and anything else is an id typed by hand.
    const int listed = m_claudeModel->findText(shown);
    if (listed >= 0) {
        return m_claudeModel->itemData(listed).toString();
    }
    return shown;
}

QString NewAgentDialog::initialPrompt() const {
    if (adapter() != QString::fromUtf8(kClaudeAdapter)) {
        return QString();
    }
    return m_initialPrompt->toPlainText().trimmed();
}

QString NewAgentDialog::runConfig() const {
    // The user data, not the text: a guessed port is spelled out in the label
    // and is no part of the name the daemon knows the configuration by.
    return m_runConfig->currentData().toString();
}

bool NewAgentDialog::initIfMissing() const { return m_initIfMissing; }

QString NewAgentDialog::group() const {
    // Reading the combo text rather than the line edit's visibility keeps this
    // correct after `exec()` returns, when every child widget is hidden again.
    if (m_group->currentText() == QString::fromUtf8(kNewGroupEntry)) {
        return m_newGroup->text().trimmed();
    }
    return m_group->currentText();
}

void NewAgentDialog::buildRecentMenu() {
    QMenu* menu = m_recent->menu();
    menu->clear();
    const QJsonArray paths =
        QJsonDocument::fromJson(m_controller->recentRepos().toUtf8()).array();
    for (const QJsonValue& value : paths) {
        const QString path = value.toString();
        if (path.isEmpty()) {
            continue;
        }
        // The path as recorded, which is the distro path the daemon was given.
        // `repoPath()` converts a Windows path on the way out and leaves one
        // that is already a distro path alone, so putting it back in the field
        // round-trips.
        menu->addAction(path, this, [this, path] {
            m_repoPath->setText(path);
            inspectRepo();
        });
    }
    m_recent->setEnabled(!menu->isEmpty());
}

void NewAgentDialog::browse() {
    const QString dir = QFileDialog::getExistingDirectory(this, "Select repository", m_repoPath->text());
    if (dir.isEmpty()) {
        return;
    }
    m_repoPath->setText(QDir::toNativeSeparators(dir));
    inspectRepo();
}

void NewAgentDialog::inspectRepo() {
    const QString path = repoPath();
    // The same path again is a no-op *unless* the last attempt at it failed:
    // that one is a retry, and a daemon that was down when the dialog opened is
    // exactly the case that needs one.
    if (path.isEmpty() || (path == m_pendingPath && !m_inspectFailed)) {
        return;
    }
    m_pendingPath = path;
    m_inspectPending = true;
    m_inspectFailed = false;
    // Both notes belong to the repository that was inspected, so they go down
    // with it rather than surviving into the answer for another one -- or into
    // no answer at all, if the inspection fails.
    m_networkNote->clear();
    m_networkNote->hide();
    m_repoState->clear();
    m_repoState->hide();
    m_initIfMissing = false;
    m_status->setText(QStringLiteral("Reading repository %1…").arg(path));
    updateOkEnabled();
    m_controller->inspectRepo(path);
    // Alongside, not after: the two answers are independent and the run
    // configurations are not worth another round trip's delay.
    m_controller->detectRunConfigs(path);
}

void NewAgentDialog::onRepoInspected(const QString& path, const QString& infoJson) {
    if (path != m_pendingPath) {
        return;
    }
    m_inspectPending = false;
    m_inspectFailed = false;
    const QJsonObject info = QJsonDocument::fromJson(infoJson.toUtf8()).object();
    // A daemon too old to send these answered nothing but repositories, so an
    // absent `is_repo` is a repository. `exists` only matters when it is not.
    showRepoState(info.value("is_repo").toBool(true), info.value("exists").toBool(false));
    const QString current = m_baseBranch->currentText();
    m_baseBranch->clear();
    for (const QJsonValue& branch : info.value("branches").toArray()) {
        m_baseBranch->addItem(branch.toString());
    }
    const QString preferred = current.isEmpty() ? info.value("default_branch").toString() : current;
    const int index = m_baseBranch->findText(preferred);
    if (index >= 0) {
        m_baseBranch->setCurrentIndex(index);
    } else {
        m_baseBranch->setEditText(preferred);
    }
    // Tracked files only: the daemon asks `git status --untracked-files=no`, the
    // same question the merge guard asks, so the sentence has to say which
    // changes it counted. A build output or a scratch note lying in a working
    // directory is its ordinary state and is not announced here.
    m_status->setText(info.value("is_dirty").toBool()
                          ? QStringLiteral("Repository has uncommitted changes to tracked files.")
                          : QString());
    updateOkEnabled();
}

void NewAgentDialog::showRepoState(bool isRepo, bool exists) {
    m_initIfMissing = !isRepo;
    if (isRepo) {
        m_repoState->clear();
        m_repoState->hide();
        return;
    }
    // Said before Create, because Create is what does it. Creating a workspace
    // from a folder that is not a repository used to fail with the daemon's own
    // wording; it now succeeds, and the user is owed the sentence saying what
    // it will do to their folder.
    m_repoState->setText(exists ? "This folder is not a git repository. It will be initialised "
                                  "with an empty first commit when the agent is created."
                                : "This folder does not exist; it will be created and "
                                  "initialised.");
    m_repoState->show();
}

void NewAgentDialog::updateNameHint() {
    // The rule itself is in Rust, next to the create it guards, so this dialog
    // and the daemon cannot come to different conclusions about a name.
    const QString hint = m_controller->validateWorkspaceName(name());
    m_nameHint->setText(hint);
    m_nameHint->setVisible(!hint.isEmpty());
    updateOkEnabled();
}

void NewAgentDialog::showNetworkAllow(const QString& json) {
    const QJsonArray allow = QJsonDocument::fromJson(json.toUtf8())
                                 .object()
                                 .value("network_allow")
                                 .toArray();
    QStringList hosts;
    for (const QJsonValue& value : allow) {
        const QString host = value.toString();
        if (!host.isEmpty()) {
            hosts.append(host);
        }
    }
    if (hosts.isEmpty()) {
        m_networkNote->clear();
        m_networkNote->hide();
        return;
    }
    // Creating the workspace applies this list; saying so here is the last
    // point at which anyone sees it before it takes effect.
    m_networkNote->setText(
        QStringLiteral("This repository adds %1 host%2 to the network allowlist: %3")
            .arg(hosts.size())
            .arg(hosts.size() == 1 ? QString() : QStringLiteral("s"), hosts.join(QStringLiteral(", "))));
    m_networkNote->show();
}

void NewAgentDialog::onRunConfigsDetected(const QString& path, const QString& json) {
    // A late answer for a repository the user has since edited away from would
    // offer configurations from the wrong tree.
    if (path != m_pendingPath) {
        return;
    }
    const QString current = runConfig();
    m_runConfig->clear();
    m_runConfig->addItem(kNoRunConfig, QString());
    runpanel::appendConfigItems(m_runConfig, json);
    showNetworkAllow(json);
    // The first runnable configuration is the offer, matching what the Run
    // panel preselects once the workspace exists. A re-detection that still has
    // what the user picked keeps it.
    const int previous = current.isEmpty() ? -1 : m_runConfig->findData(current);
    if (previous >= 0) {
        m_runConfig->setCurrentIndex(previous);
        return;
    }
    for (int i = 1; i < m_runConfig->count(); ++i) {
        if (m_runConfig->model()->flags(m_runConfig->model()->index(i, 0)) & Qt::ItemIsEnabled) {
            m_runConfig->setCurrentIndex(i);
            return;
        }
    }
}

void NewAgentDialog::onRepoInspectFailed(const QString& path, const QString& message) {
    if (path != m_pendingPath) {
        // An inspection of a repository this dialog has moved off, or one the
        // window asked for on its own account. Acting on it would grey Create
        // out over an answer about somewhere else.
        return;
    }
    // The path is kept, not cleared: an inspection that failed is still the
    // one this dialog is showing, and `updateOkEnabled` refuses Create while
    // it stands. `inspectRepo` lets the same path be asked about again once
    // this flag is up, so Enter, Recent or Browse are all a retry.
    m_inspectPending = false;
    m_inspectFailed = true;
    m_status->setText(message);
    updateOkEnabled();
}

void NewAgentDialog::onRunConfigsFailed(const QString& op, const QString& message) {
    // A daemon that cannot detect run configurations is not a reason to refuse
    // to create the workspace: the list stays at "(none)" and says why. The
    // comparison stays because this is the catch-all and the detection is the
    // one operation on it with no signal of its own; everything this dialog
    // gates on arrives typed.
    if (op == QLatin1String("repo.detect_run_configs")) {
        m_runConfig->setToolTip(message);
    }
}

void NewAgentDialog::onAdapterChanged() {
    const bool claude = adapter() == QString::fromUtf8(kClaudeAdapter);
    m_form->setRowVisible(m_command, !claude);
    m_form->setRowVisible(m_claudeModel, claude);
    m_form->setRowVisible(m_permissionMode, claude);
    m_form->setRowVisible(m_initialPrompt, claude);
}

void NewAgentDialog::onGroupChanged(int index) {
    const bool isNew = m_group->itemText(index) == QString::fromUtf8(kNewGroupEntry);
    m_newGroupLabel->setVisible(isNew);
    m_newGroup->setVisible(isNew);
    updateOkEnabled();
}

void NewAgentDialog::updateOkEnabled() {
    // Create needs an answer about the path that is in the box *now*. An
    // inspection still out means the branch combo holds the previous
    // repository's branches, one that failed means nothing in it is known to
    // exist, and a path edited since the last inspection has never been asked
    // about at all. Each of the three creates a workspace forked off a branch
    // the user did not choose.
    const QString path = repoPath();
    const bool ok = !path.isEmpty() && path == m_pendingPath && !m_inspectPending &&
                    !m_inspectFailed && !baseBranch().isEmpty() && !group().isEmpty() &&
                    m_controller->validateWorkspaceName(name()).isEmpty();
    m_buttons->button(QDialogButtonBox::Ok)->setEnabled(ok);
}

// --- offscreen test entries --------------------------------------------------
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)
#include <cstdint>

namespace {

/// The repository the dialog under test opens on. A distro path, which
/// `wslPath` passes through unchanged, so `repoPath()` answers what was typed.
const char* const kDialogRepo = "/repo/alpha";
/// A repository the dialog is not showing.
const char* const kOtherRepo = "/repo/other";

/// The dialog's status line.
QLabel* statusLabel(const NewAgentDialog& dialog) {
    return dialog.findChild<QLabel*>(QStringLiteral("NewAgentStatus"));
}

/// Whether the dialog is offering Create.
bool createEnabled(const NewAgentDialog& dialog) {
    auto* buttons = dialog.findChild<QDialogButtonBox*>();
    return buttons != nullptr && buttons->button(QDialogButtonBox::Ok)->isEnabled();
}

/// Whether the dialog is saying it is reading a repository.
bool isReading(const NewAgentDialog& dialog) {
    const QLabel* status = statusLabel(dialog);
    return status != nullptr && status->text().startsWith(QLatin1String("Reading repository"));
}

} // namespace

/// A failed inspection reaches the dialog that asked for it, and only that one.
///
/// The dialog used to learn about this by comparing the daemon's method name on
/// the catch-all `operationFailed`, which carries no path: it could not tell an
/// inspection of its own repository from one of another, and it would have lost
/// its only failure path altogether once the paired emission goes.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_takes_its_own_inspect_failure() {
    AppController controller;
    GroupModel model;
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));

    // The constructor asked, so the dialog is waiting and Create is refused
    // until the branch list is in.
    if (!isReading(dialog)) {
        return 1;
    }
    if (createEnabled(dialog)) {
        return 2;
    }

    // A failure for a repository this dialog is not showing. The window asks
    // about others; none of them is this dialog's business.
    controller.repoInspectFailed(QString::fromUtf8(kOtherRepo), QStringLiteral("not mine"));
    if (!isReading(dialog)) {
        return 3;
    }

    // Its own. The reason is shown, and Create stays refused because nothing in
    // the branch combo is known to exist.
    controller.repoInspectFailed(QString::fromUtf8(kDialogRepo), QStringLiteral("boom"));
    const QLabel* status = statusLabel(dialog);
    if (status == nullptr || status->text() != QLatin1String("boom")) {
        return 4;
    }
    if (createEnabled(dialog)) {
        return 5;
    }

    // And the retry is armed: the same path asked about again is a fresh
    // inspection rather than the no-op it would be after a success. Enter in
    // the path field is one of the three ways a user does that.
    auto* path = dialog.findChild<QLineEdit*>(QStringLiteral("NewAgentRepoPath"));
    if (path == nullptr) {
        return 6;
    }
    emit path->editingFinished();
    if (!isReading(dialog)) {
        return 7;
    }
    return 0;
}

/// What the user asked for is an agent, so the dialog offers one.
///
/// The adapter used to be chosen by looking for "claude" in the daemon's
/// capabilities, which arrive with the `hello` reply -- and this dialog is
/// built the moment the user asks for it, which can be before that lands. The
/// answer was then "no Claude here", the form came up on Terminal, and a user
/// who pressed Create got a shell in a workspace they had asked for an agent
/// in.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_offers_claude_before_the_daemon_answers() {
    AppController controller;
    GroupModel model;
    // No `hello` has been answered, so the controller has no capabilities --
    // exactly the state the dialog used to read as "Claude is unavailable".
    if (!controller.capabilitiesJson().isEmpty()) {
        return 1;
    }
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));
    if (dialog.adapter() != QLatin1String("claude")) {
        return 2;
    }

    // The model row is a list now, not a field to be told an id by heart. What
    // goes on the wire is the id behind the label.
    auto* models = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentModel"));
    if (models == nullptr || models->count() < 2) {
        return 3;
    }
    const QJsonObject none = QJsonDocument::fromJson(dialog.optionsJson().toUtf8()).object();
    if (none.contains(QStringLiteral("model"))) {
        // The first entry is Claude Code's own default, which is sent by not
        // being sent: "" is not a model name.
        return 4;
    }
    models->setCurrentIndex(1);
    const QJsonObject listed = QJsonDocument::fromJson(dialog.optionsJson().toUtf8()).object();
    if (!listed.value(QStringLiteral("model")).toString().startsWith(QLatin1String("claude-"))) {
        return 5;
    }

    // And a model that is not on the list is still one keystroke away: the CLI
    // takes any name, so a new one must not need a new build of the IDE.
    models->setCurrentText(QStringLiteral("claude-something-new"));
    const QJsonObject typed = QJsonDocument::fromJson(dialog.optionsJson().toUtf8()).object();
    if (typed.value(QStringLiteral("model")).toString() != QLatin1String("claude-something-new")) {
        return 6;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
