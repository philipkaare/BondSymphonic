#include "NewAgentDialog.h"
#include "RunPanel.h"
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
#include <QPlainTextEdit>
#include <QPushButton>
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

/// Whether the daemon said it has the Claude adapter, from the capabilities it
/// reported in `hello`. Offering Claude Code by default against a daemon that
/// cannot run it would make the dialog's first suggestion its only dead end.
bool daemonHasClaude(AppController* controller) {
    const QJsonArray adapters = QJsonDocument::fromJson(controller->capabilitiesJson().toUtf8())
                                    .object()
                                    .value("adapters")
                                    .toArray();
    return adapters.contains(QJsonValue(QString::fromUtf8(kClaudeAdapter)));
}

} // namespace

NewAgentDialog::NewAgentDialog(AppController* controller, GroupModel* model, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_model(model) {
    setWindowTitle("New Agent");
    setModal(true);

    auto* outer = new QVBoxLayout(this);
    m_form = new QFormLayout();
    QFormLayout* form = m_form;
    outer->addLayout(form);

    auto* repoRow = new QHBoxLayout();
    m_repoPath = new QLineEdit(this);
    m_repoPath->setPlaceholderText("C:\\git\\project or /home/you/project");
    auto* browseButton = new QPushButton("Browse…", this);
    repoRow->addWidget(m_repoPath, 1);
    repoRow->addWidget(browseButton, 0);
    form->addRow("Repository:", repoRow);

    m_baseBranch = new QComboBox(this);
    m_baseBranch->setEditable(true);
    form->addRow("Base branch:", m_baseBranch);

    int tabs = 0;
    for (int g = 0; g < m_model->groupCount(); ++g) {
        tabs += m_model->tabCount(g);
    }
    m_name = new QLineEdit(QString("agent-%1").arg(tabs + 1), this);
    form->addRow("Name:", m_name);

    m_adapter = new QComboBox(this);
    // Claude Code first, so it is the default: it is what the IDE is for, and
    // a terminal workspace is the fallback rather than the usual case.
    m_adapter->addItem("Claude Code", kClaudeAdapter);
    m_adapter->addItem("Terminal", kTerminalAdapter);
    m_adapter->setCurrentIndex(daemonHasClaude(m_controller) ? 0 : 1);
    form->addRow("Adapter:", m_adapter);

    m_command = new QLineEdit(this);
    m_command->setPlaceholderText("default shell");
    form->addRow("Command:", m_command);

    m_claudeModel = new QLineEdit(this);
    m_claudeModel->setPlaceholderText("default");
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
    m_status->setWordWrap(true);
    outer->addWidget(m_status);

    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText("Create");
    outer->addWidget(m_buttons);

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
    QObject::connect(m_controller, &AppController::operationFailed, this, &NewAgentDialog::onInspectFailed);

    onAdapterChanged();
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
    const QString model = m_claudeModel->text().trimmed();
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

QString NewAgentDialog::group() const {
    // Reading the combo text rather than the line edit's visibility keeps this
    // correct after `exec()` returns, when every child widget is hidden again.
    if (m_group->currentText() == QString::fromUtf8(kNewGroupEntry)) {
        return m_newGroup->text().trimmed();
    }
    return m_group->currentText();
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
    if (path.isEmpty() || path == m_pendingPath) {
        return;
    }
    m_pendingPath = path;
    m_status->setText(QString("Inspecting %1…").arg(path));
    m_controller->inspectRepo(path);
    // Alongside, not after: the two answers are independent and the run
    // configurations are not worth another round trip's delay.
    m_controller->detectRunConfigs(path);
}

void NewAgentDialog::onRepoInspected(const QString& path, const QString& infoJson) {
    if (path != m_pendingPath) {
        return;
    }
    const QJsonObject info = QJsonDocument::fromJson(infoJson.toUtf8()).object();
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
    m_status->setText(info.value("is_dirty").toBool() ? "Repository has uncommitted changes." : QString());
    updateOkEnabled();
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

void NewAgentDialog::onInspectFailed(const QString& op, const QString& message) {
    // Only inspection failures belong in this dialog; the window reports the rest.
    if (op == "repo.inspect") {
        m_pendingPath.clear();
        m_status->setText(message);
        return;
    }
    // A daemon that cannot detect run configurations is not a reason to refuse
    // to create the workspace: the list stays at "(none)" and says why.
    if (op == "repo.detect_run_configs") {
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
    const bool ok = !repoPath().isEmpty() && !baseBranch().isEmpty() && !group().isEmpty();
    m_buttons->button(QDialogButtonBox::Ok)->setEnabled(ok);
}
