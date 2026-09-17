#include "NewAgentDialog.h"
#include "AgentChoices.h"
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
#include <QProgressBar>
#include <QPushButton>
#include <QRadioButton>
#include <QSignalBlocker>
#include <QStringList>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// The entry that turns the group combo into a free-text field.
const char* kNewGroupEntry = "New group…";

/// The adapters as the daemon spells them.
const char* kClaudeAdapter = "claude";
const char* kTerminalAdapter = "terminal";

/// How wide the busy bar beside the base-branch combo is. Narrow enough that
/// the combo keeps the row, wide enough for the sweep to read as motion.
constexpr int kBranchBusyWidth = 16;

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

    // Where the agent works. A worktree is the default: it is the choice that
    // keeps the agent's work off the user's own branch.
    auto* modes = new QWidget(this);
    auto* modesLayout = new QVBoxLayout(modes);
    modesLayout->setContentsMargins(0, 0, 0, 0);
    m_worktreeMode = new QRadioButton(QStringLiteral("Work in a new worktree"), modes);
    m_worktreeMode->setObjectName(QStringLiteral("NewAgentWorktreeMode"));
    m_inPlaceMode = new QRadioButton(QStringLiteral("Work directly in this checkout"), modes);
    m_inPlaceMode->setObjectName(QStringLiteral("NewAgentInPlaceMode"));
    modesLayout->addWidget(m_worktreeMode);
    modesLayout->addWidget(m_inPlaceMode);
    (m_controller->newAgentInPlace() ? m_inPlaceMode : m_worktreeMode)->setChecked(true);
    form->addRow("Work in:", modes);

    m_inPlaceHelp = new QLabel(QStringLiteral("The agent edits this folder on its current branch. "
                                              "Its changes are not isolated on a branch of their "
                                              "own."),
                               this);
    m_inPlaceHelp->setObjectName(QStringLiteral("NewAgentInPlaceHelp"));
    m_inPlaceHelp->setWordWrap(true);
    m_inPlaceHelp->setTextFormat(Qt::PlainText);
    form->addRow(QString(), m_inPlaceHelp);

    m_hooksWarning = new QLabel(this);
    m_hooksWarning->setObjectName(QStringLiteral("NewAgentHooksWarning"));
    m_hooksWarning->setWordWrap(true);
    m_hooksWarning->setTextFormat(Qt::PlainText);
    // The same red as the name hint: this is a thing that can go wrong.
    m_hooksWarning->setStyleSheet(
        QStringLiteral("color:%1")
            .arg(theme::ink(theme::removed(), theme::isDark(palette())).name()));
    form->addRow(QString(), m_hooksWarning);

    m_baseBranch = new QComboBox(this);
    m_baseBranch->setObjectName(QStringLiteral("NewAgentBaseBranch"));
    m_baseBranch->setEditable(true);
    m_branchBusy = new QProgressBar(this);
    // A range of nothing is Qt's indeterminate bar: there is no progress to
    // report, only that something is still out.
    m_branchBusy->setRange(0, 0);
    m_branchBusy->setTextVisible(false);
    m_branchBusy->setMaximumWidth(kBranchBusyWidth);
    auto* branchRow = new QHBoxLayout();
    branchRow->setContentsMargins(0, 0, 0, 0);
    branchRow->addWidget(m_baseBranch, 1);
    branchRow->addWidget(m_branchBusy);
    form->addRow("Base branch:", branchRow);
    // The settled state, so the bar is not on screen for the instant before the
    // constructor's own inspection puts it there.
    updateBranchState();

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
    m_claudeModel->setToolTip("Which model the agent runs. Anything Claude Code accepts can be "
                              "typed here as well.");
    // Filled from the one list, which makes it editable: a model that is not on
    // it is still one keystroke away, and `optionsJson` reads the text rather
    // than the selection because of that. Nothing typed is added to the list --
    // an id that only ever existed in this dialog would outlive the dialog and
    // be offered to the next agent as though somebody had chosen it.
    agentchoices::fillModelCombo(m_claudeModel, QString());
    m_claudeModel->setInsertPolicy(QComboBox::NoInsert);
    form->addRow("Model:", m_claudeModel);

    m_permissionMode = new QComboBox(this);
    m_permissionMode->setObjectName(QStringLiteral("NewAgentPermissionMode"));
    m_permissionMode->setToolTip("What this agent asks before it acts. Settings is where the "
                                 "answer a new agent starts on is chosen.");
    // Seeded from the setting rather than from the first entry, because the
    // setting exists precisely to say what a new agent should start on.
    agentchoices::fillPermissionCombo(m_permissionMode, m_controller->defaultPermissionMode());
    form->addRow("Permissions:", m_permissionMode);

    auto* permissionNote = new QLabel(agentchoices::permissionNote(), this);
    permissionNote->setObjectName(QStringLiteral("NewAgentPermissionNote"));
    permissionNote->setWordWrap(true);
    permissionNote->setEnabled(false);
    form->addRow(QString(), permissionNote);

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
    QObject::connect(m_inPlaceMode, &QRadioButton::toggled, this, [this](bool checked) {
        // The branch picked for a worktree is kept for the way back, since the
        // row is about to show the checkout's branch instead.
        if (checked && m_baseBranch->isEnabled()) {
            m_branchChoice = m_baseBranch->currentText();
        }
        updateModeState();
        updateOkEnabled();
    });

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
    updateModeState();
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

QString NewAgentDialog::baseBranch() const {
    return inPlace() ? QString() : m_baseBranch->currentText().trimmed();
}

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
    // Always, never conditionally. A permission mode left out is the CLI's own
    // default -- a fifth behaviour nobody chose, that nothing in the IDE can
    // show and that the combo the user just looked at does not describe.
    options.insert("permission_mode", m_permissionMode->currentData().toString());
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

bool NewAgentDialog::inPlace() const {
    return m_inPlaceMode->isChecked() && inPlaceAvailable();
}

bool NewAgentDialog::inPlaceAvailable() const {
    return m_inPlaceRefusal.isEmpty() && !m_inspectPending && !m_inspectFailed;
}

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
    // Taken before the row goes into its loading state, which is about to
    // replace whatever is in the combo, and only from a combo that is actually
    // offering branches: "Loading branches…" and "Could not read branches" are
    // the row's own words and were never a choice the user made.
    if (m_baseBranch->isEnabled() && !m_inPlaceMode->isChecked()) {
        m_branchChoice = m_baseBranch->currentText();
    }
    m_inspectPending = true;
    m_inspectFailed = false;
    // What the last inspection said about working in place was about the last
    // repository, like the notes below.
    m_inPlaceRefusal.clear();
    m_hooksPath.clear();
    m_headBranch.clear();
    // Both notes belong to the repository that was inspected, so they go down
    // with it rather than surviving into the answer for another one -- or into
    // no answer at all, if the inspection fails.
    m_networkNote->clear();
    m_networkNote->hide();
    m_repoState->clear();
    m_repoState->hide();
    m_initIfMissing = false;
    m_status->setText(QStringLiteral("Reading repository %1…").arg(path));
    updateBranchState();
    updateModeState();
    updateOkEnabled();
    m_controller->inspectRepo(path);
    // Alongside, not after: the two answers are independent and the run
    // configurations are not worth another round trip's delay.
    m_controller->detectRunConfigs(path);
}

void NewAgentDialog::updateBranchState() {
    if (m_inspectPending) {
        m_baseBranch->setEnabled(false);
        m_baseBranch->clear();
        // An item rather than a placeholder: a placeholder is invisible on an
        // editable combo whose edit is empty, which is exactly this state.
        m_baseBranch->addItem(QStringLiteral("Loading branches…"));
        m_branchBusy->show();
        return;
    }
    m_branchBusy->hide();
    if (m_inspectFailed) {
        m_baseBranch->setEnabled(false);
        m_baseBranch->clear();
        m_baseBranch->addItem(QStringLiteral("Could not read branches"));
        return;
    }
    m_baseBranch->setEnabled(true);
}

void NewAgentDialog::onRepoInspected(const QString& path, const QString& infoJson) {
    if (path != m_pendingPath) {
        return;
    }
    m_inspectPending = false;
    m_inspectFailed = false;
    // Before the combo is filled, not after: this is what takes the loading
    // item back out, and a branch list added on top of it would be one entry
    // longer than the repository has branches.
    updateBranchState();
    const QJsonObject info = QJsonDocument::fromJson(infoJson.toUtf8()).object();
    // A daemon too old to send these answered nothing but repositories, so an
    // absent `is_repo` is a repository. `exists` only matters when it is not.
    showRepoState(info.value("is_repo").toBool(true), info.value("exists").toBool(false));
    m_branches.clear();
    for (const QJsonValue& value : info.value("branches").toArray()) {
        m_branches.append(value.toString());
    }
    m_defaultBranch = info.value("default_branch").toString();
    // A folder about to be initialised will be on `main`, which is also what
    // `default_branch` says for one.
    m_headBranch = info.value("is_repo").toBool(true) ? info.value("head_branch").toString()
                                                     : m_defaultBranch;
    m_inPlaceRefusal = info.value("in_place_refusal").toString();
    m_hooksPath = info.value("hooks_path_in_tree").toString();
    updateModeState();
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
    updateBranchState();
    updateModeState();
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

void NewAgentDialog::updateModeState() {
    m_inPlaceMode->setEnabled(m_inPlaceRefusal.isEmpty());
    m_inPlaceMode->setToolTip(m_inPlaceRefusal);
    if (!m_inPlaceRefusal.isEmpty() && m_inPlaceMode->isChecked()) {
        // Said by the tooltip on the disabled choice; the daemon would refuse
        // the create with the same sentence.
        m_worktreeMode->setChecked(true);
    }
    const bool inPlace = m_inPlaceMode->isChecked();
    m_form->setRowVisible(m_inPlaceHelp, inPlace);
    m_hooksWarning->setText(
        m_hooksPath.isEmpty()
            ? QString()
            : QStringLiteral("This repository runs git hooks from %1 inside the working tree. "
                             "The agent can change them, and they run outside the sandbox the "
                             "next time you use git here.")
                  .arg(m_hooksPath));
    m_form->setRowVisible(m_hooksWarning, inPlace && !m_hooksPath.isEmpty());
    if (m_inspectPending || m_inspectFailed) {
        // `updateBranchState` owns the row while an answer is out or missing.
        return;
    }
    const QSignalBlocker quiet(m_baseBranch);
    m_baseBranch->clear();
    if (inPlace) {
        // Shown, not chosen: nothing is switched and nothing is sent.
        m_baseBranch->addItem(m_headBranch.isEmpty() ? QStringLiteral("detached HEAD")
                                                     : m_headBranch);
        m_baseBranch->setEnabled(false);
        return;
    }
    m_baseBranch->setEnabled(true);
    m_baseBranch->addItems(m_branches);
    const QString preferred = m_branchChoice.isEmpty() ? m_defaultBranch : m_branchChoice;
    const int index = m_baseBranch->findText(preferred);
    if (index >= 0) {
        m_baseBranch->setCurrentIndex(index);
    } else {
        m_baseBranch->setEditText(preferred);
    }
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
                    !m_inspectFailed && (inPlace() || !baseBranch().isEmpty()) && !group().isEmpty() &&
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

/// The Base branch combo says which of its three states it is in.
///
/// An empty enabled combo is indistinguishable from a repository with no
/// branches, and `repo.inspect` on a cold or remote repository takes seconds to
/// answer. Create refuses to act through both the pending and the failed state
/// already; this is those two states said where the user is looking.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_says_branches_are_loading() {
    AppController controller;
    GroupModel model;
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));
    auto* combo = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentBaseBranch"));
    if (combo == nullptr) {
        return 1;
    }

    // The constructor inspected, so the dialog is waiting for the branch list.
    if (combo->isEnabled()) {
        return 2;
    }
    if (!combo->currentText().contains(QLatin1String("Loading"))) {
        return 3;
    }

    controller.repoInspected(
        QString::fromUtf8(kDialogRepo),
        QStringLiteral(R"({"is_repo":true,"branches":["main","dev"],"default_branch":"main"})"));
    if (!combo->isEnabled() || combo->count() != 2 ||
        combo->currentText() != QLatin1String("main")) {
        // The loading item has to be gone before the branches go in, or it is
        // a third entry in a list of two.
        return 4;
    }

    auto* path = dialog.findChild<QLineEdit*>(QStringLiteral("NewAgentRepoPath"));
    if (path == nullptr) {
        return 5;
    }
    path->setText(QString::fromUtf8(kOtherRepo));
    emit path->editingFinished();
    if (combo->isEnabled()) {
        return 6;
    }
    controller.repoInspectFailed(QString::fromUtf8(kOtherRepo), QStringLiteral("boom"));
    if (combo->isEnabled()) {
        return 7;
    }
    if (!combo->currentText().contains(QLatin1String("Could not read"))) {
        return 8;
    }
    return 0;
}

/// A new agent leaves this dialog with a permission mode on it, and the mode is
/// one of the four the IDE offers.
///
/// The dialog used to write the mode only when it was non-empty, out of a list
/// that still held `default` and `dontAsk`. Both halves were wrong in the same
/// direction: an option left out is the CLI's own default, `dontAsk` denies in
/// silence, and each of those is an agent behaving in a way nobody chose and
/// nothing on screen describes.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_always_sends_a_permission_mode() {
    AppController controller;
    GroupModel model;
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));

    auto* modes = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentPermissionMode"));
    if (modes == nullptr) {
        return 1;
    }
    if (modes->count() != agentchoices::permissionModes().size()) {
        // The dialog kept a list of its own, which is a list free to drift.
        return 2;
    }
    for (int i = 0; i < modes->count(); ++i) {
        if (modes->itemData(i).toString() !=
            QString::fromUtf8(agentchoices::permissionModes().at(i).id)) {
            return 3;
        }
    }

    // Untouched, and still on the wire: what the dialog opened on is a choice
    // as much as one the user made by hand.
    const QJsonObject untouched = QJsonDocument::fromJson(dialog.optionsJson().toUtf8()).object();
    if (!untouched.contains(QStringLiteral("permission_mode"))) {
        return 4;
    }
    if (untouched.value(QStringLiteral("permission_mode")).toString() !=
        modes->currentData().toString()) {
        return 5;
    }

    modes->setCurrentIndex(modes->findData(QStringLiteral("bypassPermissions")));
    const QJsonObject yolo = QJsonDocument::fromJson(dialog.optionsJson().toUtf8()).object();
    if (yolo.value(QStringLiteral("permission_mode")).toString() !=
        QLatin1String("bypassPermissions")) {
        return 6;
    }
    return 0;
}

/// The dialog offers the checkout itself, says what that means, warns about
/// hooks the agent could edit, and will not offer it where the daemon would
/// refuse.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_offers_the_checkout_itself() {
    AppController controller;
    GroupModel model;
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));
    auto* worktree = dialog.findChild<QRadioButton*>(QStringLiteral("NewAgentWorktreeMode"));
    auto* inPlace = dialog.findChild<QRadioButton*>(QStringLiteral("NewAgentInPlaceMode"));
    auto* help = dialog.findChild<QLabel*>(QStringLiteral("NewAgentInPlaceHelp"));
    auto* warning = dialog.findChild<QLabel*>(QStringLiteral("NewAgentHooksWarning"));
    auto* branch = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentBaseBranch"));
    if (worktree == nullptr || inPlace == nullptr || help == nullptr || warning == nullptr ||
        branch == nullptr) {
        return 1;
    }
    controller.repoInspected(
        QString::fromUtf8(kDialogRepo),
        QStringLiteral(R"({"is_repo":true,"branches":["main","feature"],"default_branch":"main",)"
                       R"("head_branch":"feature","hooks_path_in_tree":".husky/_"})"));
    // A worktree is the default, with the branches offered.
    if (!worktree->isChecked() || dialog.inPlace() || !help->isHidden() || !warning->isHidden() ||
        !branch->isEnabled() || branch->currentText() != QLatin1String("main")) {
        return 2;
    }

    inPlace->setChecked(true);
    if (!dialog.inPlace() || help->isHidden() || warning->isHidden()) {
        return 3;
    }
    if (help->text() != QLatin1String("The agent edits this folder on its current branch. Its "
                                      "changes are not isolated on a branch of their own.")) {
        return 4;
    }
    if (!warning->text().contains(QLatin1String(".husky/_")) ||
        !warning->text().contains(QLatin1String("outside the sandbox"))) {
        return 5;
    }
    // The branch is the checked-out one, shown and not chosen, and not sent.
    if (branch->isEnabled() || branch->currentText() != QLatin1String("feature") ||
        !dialog.baseBranch().isEmpty()) {
        return 6;
    }
    // Back to a worktree: the list comes back, with the user's choice.
    worktree->setChecked(true);
    if (!branch->isEnabled() || branch->count() != 2) {
        return 7;
    }

    // A detached HEAD says so.
    inPlace->setChecked(true);
    controller.repoInspected(QString::fromUtf8(kDialogRepo),
                             QStringLiteral(R"({"is_repo":true,"branches":["main"],)"
                                            R"("default_branch":"main"})"));
    if (branch->currentText() != QLatin1String("detached HEAD") || !warning->isHidden()) {
        return 8;
    }

    // A linked worktree cannot be worked in place: the choice is off, says
    // why, and the dialog falls back to a worktree.
    controller.repoInspected(
        QString::fromUtf8(kDialogRepo),
        QStringLiteral(R"({"is_repo":true,"branches":["main"],"default_branch":"main",)"
                       R"("head_branch":"main","in_place_refusal":"it is a linked worktree"})"));
    if (inPlace->isEnabled() || dialog.inPlace() || dialog.inPlaceAvailable() ||
        !inPlace->toolTip().contains(QLatin1String("linked worktree"))) {
        return 9;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
