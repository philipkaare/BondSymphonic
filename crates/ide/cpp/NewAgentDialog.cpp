#include "NewAgentDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QDir>
#include <QFileDialog>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

/// The entry that turns the group combo into a free-text field.
const char* kNewGroupEntry = "New group…";

} // namespace

NewAgentDialog::NewAgentDialog(AppController* controller, GroupModel* model, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_model(model) {
    setWindowTitle("New Agent");
    setModal(true);

    auto* outer = new QVBoxLayout(this);
    auto* form = new QFormLayout();
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
    // Claude Code arrives in Milestone 4; until then a workspace runs a shell.
    m_adapter->addItem("Terminal", "terminal");
    form->addRow("Adapter:", m_adapter);

    m_command = new QLineEdit(this);
    m_command->setPlaceholderText("default shell");
    form->addRow("Command:", m_command);

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
    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    QObject::connect(m_controller, &AppController::repoInspected, this, &NewAgentDialog::onRepoInspected);
    QObject::connect(m_controller, &AppController::operationFailed, this, &NewAgentDialog::onInspectFailed);

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

QString NewAgentDialog::command() const { return m_command->text().trimmed(); }

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

void NewAgentDialog::onInspectFailed(const QString& op, const QString& message) {
    // Only inspection failures belong in this dialog; the window reports the rest.
    if (op == "repo.inspect") {
        m_pendingPath.clear();
        m_status->setText(message);
    }
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
