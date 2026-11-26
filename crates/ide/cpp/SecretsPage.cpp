#include "SecretsPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QRegularExpression>
#include <QTreeWidget>
#include <QVBoxLayout>
#if defined(BS_WIDGET_TESTS)
#include <cstdint>
#endif

namespace {

constexpr QChar kEllipsis(0x2026);

/// The scope combo's first entry, and the list's word for no repository.
QString everyRepository() { return QStringLiteral("All repositories"); }

/// Roles on a list row: the secret's name and its repository ("" for all).
constexpr int kNameRole = Qt::UserRole;
constexpr int kRepoRole = Qt::UserRole + 1;

} // namespace

SecretDialog::SecretDialog(const QString& name, const QString& repo, const QStringList& repos,
                           QWidget* parent)
    : QDialog(parent) {
    const bool existing = !name.isEmpty();
    setWindowTitle(existing ? QStringLiteral("Change secret") : QStringLiteral("Add secret"));
    setModal(true);
    auto* outer = new QVBoxLayout(this);
    auto* form = new QFormLayout();
    m_name = new QLineEdit(name, this);
    m_name->setObjectName(QStringLiteral("SecretName"));
    m_name->setPlaceholderText(QStringLiteral("API_TOKEN"));
    m_name->setReadOnly(existing);
    form->addRow(QStringLiteral("Name:"), m_name);
    m_value = new QLineEdit(this);
    m_value->setObjectName(QStringLiteral("SecretValue"));
    m_value->setEchoMode(QLineEdit::Password);
    m_value->setPlaceholderText(existing ? QStringLiteral("The new value")
                                         : QStringLiteral("The value"));
    form->addRow(QStringLiteral("Value:"), m_value);
    m_scope = new QComboBox(this);
    m_scope->setObjectName(QStringLiteral("SecretScope"));
    m_scope->setEditable(!existing);
    m_scope->addItem(everyRepository());
    for (const QString& r : repos) {
        if (!r.isEmpty() && m_scope->findText(r) < 0) {
            m_scope->addItem(r);
        }
    }
    if (!repo.isEmpty()) {
        if (m_scope->findText(repo) < 0) {
            m_scope->addItem(repo);
        }
        m_scope->setCurrentIndex(m_scope->findText(repo));
    }
    m_scope->setEnabled(!existing);
    m_scope->setToolTip(QStringLiteral("For every repository, or only for sandboxes of one; a "
                                       "repository's own wins over one for all of them"));
    form->addRow(QStringLiteral("For:"), m_scope);
    outer->addLayout(form);
    m_problem = new QLabel(this);
    m_problem->setObjectName(QStringLiteral("SecretProblem"));
    m_problem->setTextFormat(Qt::PlainText);
    m_problem->setWordWrap(true);
    outer->addWidget(m_problem);
    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Save"));
    outer->addWidget(m_buttons);
    QObject::connect(m_name, &QLineEdit::textChanged, this, [this] { updateEnabled(); });
    QObject::connect(m_value, &QLineEdit::textChanged, this, [this] { updateEnabled(); });
    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    updateEnabled();
    if (existing) {
        m_value->setFocus();
    }
}

QString SecretDialog::name() const { return m_name->text().trimmed(); }

QString SecretDialog::value() const { return m_value->text(); }

QString SecretDialog::repo() const {
    const QString text = m_scope->currentText().trimmed();
    return text == everyRepository() ? QString() : text;
}

QString SecretDialog::nameProblem(const QString& name) {
    static const QRegularExpression shape(QStringLiteral("^[A-Za-z_][A-Za-z0-9_]*$"));
    if (name.isEmpty()) {
        return QString();
    }
    if (!shape.match(name).hasMatch() || name.size() > 128) {
        return QStringLiteral("A name is letters, digits and _, not starting with a digit.");
    }
    static const QStringList reserved{
        "HOME",        "USER",        "LOGNAME",   "PATH",       "SHELL",       "PWD",
        "TERM",        "LANG",        "HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy",
        "ALL_PROXY",   "all_proxy",   "NO_PROXY",  "no_proxy",   "LD_PRELOAD",  "LD_LIBRARY_PATH",
        "TMPDIR",      "XDG_RUNTIME_DIR",
    };
    if (reserved.contains(name) || name.startsWith(QLatin1String("BS_")) ||
        name.startsWith(QLatin1String("GIT_"))) {
        return QStringLiteral("%1 is set by the sandbox itself.").arg(name);
    }
    return QString();
}

void SecretDialog::updateEnabled() {
    const QString problem = nameProblem(name());
    m_problem->setText(problem);
    m_problem->setVisible(!problem.isEmpty());
    m_buttons->button(QDialogButtonBox::Ok)
        ->setEnabled(!name().isEmpty() && problem.isEmpty() && !m_value->text().isEmpty());
}

SecretsPage::SecretsPage(AppController* controller, QWidget* parent)
    : QWidget(parent), m_controller(controller) {
    auto* outer = new QVBoxLayout(this);
    auto* intro = new QLabel(
        QStringLiteral(
            "Secrets are environment variables every process in an agent's sandbox gets: the "
            "agent, its terminals and runs, and local MCP servers. Set one for all repositories, "
            "or for one repository, whose own wins. A change reaches processes started after "
            "it; restart an agent for it to see the change. Values stay with the daemon and are "
            "never shown again. An agent can use whatever it can read, so add only what you "
            "would let it use."),
        this);
    intro->setWordWrap(true);
    outer->addWidget(intro);

    m_list = new QTreeWidget(this);
    m_list->setObjectName(QStringLiteral("SecretsList"));
    m_list->setColumnCount(2);
    m_list->setHeaderLabels({ QStringLiteral("Name"), QStringLiteral("For") });
    m_list->setRootIsDecorated(false);
    m_list->header()->setStretchLastSection(true);
    outer->addWidget(m_list, 1);

    auto* row = new QHBoxLayout();
    m_add = new QPushButton(QStringLiteral("Add") + kEllipsis, this);
    m_add->setObjectName(QStringLiteral("SecretsAdd"));
    m_change = new QPushButton(QStringLiteral("Change value") + kEllipsis, this);
    m_change->setObjectName(QStringLiteral("SecretsChange"));
    m_remove = new QPushButton(QStringLiteral("Remove"), this);
    m_remove->setObjectName(QStringLiteral("SecretsRemove"));
    row->addWidget(m_add);
    row->addWidget(m_change);
    row->addWidget(m_remove);
    row->addStretch(1);
    outer->addLayout(row);

    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("SecretsStatus"));
    m_status->setTextFormat(Qt::PlainText);
    m_status->setWordWrap(true);
    outer->addWidget(m_status);

    QObject::connect(m_add, &QPushButton::clicked, this, [this] { addSecret(); });
    QObject::connect(m_change, &QPushButton::clicked, this, [this] { changeSelected(); });
    QObject::connect(m_remove, &QPushButton::clicked, this, [this] { removeSelected(); });
    QObject::connect(m_list, &QTreeWidget::itemSelectionChanged, this, [this] { updateButtons(); });
    QObject::connect(m_list, &QTreeWidget::itemDoubleClicked, this, [this] { changeSelected(); });
    QObject::connect(m_controller, &AppController::secretsListed, this,
                     [this](const QString& json) {
                         m_status->clear();
                         showListing(json);
                     });
    QObject::connect(m_controller, &AppController::secretsFailed, this,
                     [this](const QString& message) { m_status->setText(message); });
    updateButtons();
    m_controller->listSecrets();
}

void SecretsPage::showListing(const QString& json) {
    m_list->clear();
    const QJsonArray rows = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& value : rows) {
        const QJsonObject secret = value.toObject();
        const QString name = secret.value(QStringLiteral("name")).toString();
        const QString repo = secret.value(QStringLiteral("repo")).toString();
        auto* item = new QTreeWidgetItem(m_list, { name, repo.isEmpty() ? everyRepository() : repo });
        item->setData(0, kNameRole, name);
        item->setData(0, kRepoRole, repo);
    }
    m_list->resizeColumnToContents(0);
    updateButtons();
}

QStringList SecretsPage::knownRepos() const {
    QStringList repos;
    const QJsonArray recent =
        QJsonDocument::fromJson(m_controller->recentRepos().toUtf8()).array();
    for (const QJsonValue& value : recent) {
        repos << value.toString();
    }
    for (int i = 0; i < m_list->topLevelItemCount(); ++i) {
        const QString repo = m_list->topLevelItem(i)->data(0, kRepoRole).toString();
        if (!repo.isEmpty() && !repos.contains(repo)) {
            repos << repo;
        }
    }
    return repos;
}

void SecretsPage::addSecret() {
    SecretDialog dialog(QString(), QString(), knownRepos(), this);
    if (dialog.exec() != QDialog::Accepted) {
        return;
    }
    QString repo = dialog.repo();
    if (!repo.isEmpty()) {
        // A Windows path is the distro path the daemon knows the repository by.
        const QString distro = m_controller->wslPath(repo);
        if (distro.isEmpty()) {
            m_status->setText(QStringLiteral("%1 is not a folder the daemon can reach.").arg(repo));
            return;
        }
        repo = distro;
    }
    m_status->setText(QStringLiteral("Saving") + kEllipsis);
    m_controller->setSecret(dialog.name(), dialog.value(), repo);
}

void SecretsPage::changeSelected() {
    QTreeWidgetItem* item = m_list->currentItem();
    if (item == nullptr) {
        return;
    }
    const QString name = item->data(0, kNameRole).toString();
    const QString repo = item->data(0, kRepoRole).toString();
    SecretDialog dialog(name, repo, knownRepos(), this);
    if (dialog.exec() != QDialog::Accepted) {
        return;
    }
    m_status->setText(QStringLiteral("Saving") + kEllipsis);
    m_controller->setSecret(name, dialog.value(), repo);
}

void SecretsPage::removeSelected() {
    QTreeWidgetItem* item = m_list->currentItem();
    if (item == nullptr) {
        return;
    }
    const QString name = item->data(0, kNameRole).toString();
    const QString repo = item->data(0, kRepoRole).toString();
    const QString scope = repo.isEmpty() ? QStringLiteral("all repositories") : repo;
    if (QMessageBox::question(this, QStringLiteral("Remove secret"),
                              QStringLiteral("Remove %1 for %2? Processes already running keep "
                                             "it until they are restarted.")
                                  .arg(name, scope)) != QMessageBox::Yes) {
        return;
    }
    m_controller->removeSecret(name, repo);
}

void SecretsPage::updateButtons() {
    const bool selected = m_list->currentItem() != nullptr && !m_list->selectedItems().isEmpty();
    m_change->setEnabled(selected);
    m_remove->setEnabled(selected);
}

#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

/// The page lists names and scopes from the daemon's answer and offers Change
/// and Remove only for a chosen row; the dialog refuses a name the daemon
/// would, keeps an existing secret's name and scope fixed, and reads back
/// what it was given.
extern "C" std::int32_t bs_widget_test_secrets_page_lists_and_validates() {
    if (!SecretDialog::nameProblem(QStringLiteral("1BAD")).contains(QStringLiteral("letters")) ||
        !SecretDialog::nameProblem(QStringLiteral("PATH")).contains(QStringLiteral("sandbox")) ||
        !SecretDialog::nameProblem(QStringLiteral("GIT_DIR")).contains(QStringLiteral("sandbox")) ||
        !SecretDialog::nameProblem(QStringLiteral("API_TOKEN")).isEmpty()) {
        return 1;
    }
    AppController controller;
    SecretsPage page(&controller);
    auto* list = page.findChild<QTreeWidget*>(QStringLiteral("SecretsList"));
    auto* change = page.findChild<QPushButton*>(QStringLiteral("SecretsChange"));
    auto* remove = page.findChild<QPushButton*>(QStringLiteral("SecretsRemove"));
    if (list == nullptr || change == nullptr || remove == nullptr) {
        return 2;
    }
    controller.secretsListed(QStringLiteral(
        R"([{"name":"NPM_TOKEN"},{"name":"API_KEY","repo":"/home/me/git/app"}])"));
    if (list->topLevelItemCount() != 2 ||
        list->topLevelItem(0)->text(1) != QLatin1String("All repositories") ||
        list->topLevelItem(1)->text(1) != QLatin1String("/home/me/git/app")) {
        return 3;
    }
    if (change->isEnabled() || remove->isEnabled()) {
        return 4;
    }
    list->setCurrentItem(list->topLevelItem(1));
    if (!change->isEnabled() || !remove->isEnabled()) {
        return 5;
    }
    controller.secretsFailed(QStringLiteral("PATH is set by the sandbox itself"));
    auto* status = page.findChild<QLabel*>(QStringLiteral("SecretsStatus"));
    if (status == nullptr || !status->text().contains(QStringLiteral("PATH"))) {
        return 6;
    }

    SecretDialog fresh(QString(), QString(), { QStringLiteral("/home/me/git/app") });
    auto* name = fresh.findChild<QLineEdit*>(QStringLiteral("SecretName"));
    auto* value = fresh.findChild<QLineEdit*>(QStringLiteral("SecretValue"));
    auto* scope = fresh.findChild<QComboBox*>(QStringLiteral("SecretScope"));
    auto* buttons = fresh.findChild<QDialogButtonBox*>();
    if (name == nullptr || value == nullptr || scope == nullptr || buttons == nullptr) {
        return 7;
    }
    QPushButton* save = buttons->button(QDialogButtonBox::Ok);
    if (value->echoMode() != QLineEdit::Password || save->isEnabled()) {
        return 8;
    }
    name->setText(QStringLiteral("PATH"));
    value->setText(QStringLiteral("x"));
    if (save->isEnabled()) {
        return 9;
    }
    name->setText(QStringLiteral("API_TOKEN"));
    if (!save->isEnabled() || !fresh.repo().isEmpty()) {
        return 10;
    }
    scope->setCurrentIndex(scope->findText(QStringLiteral("/home/me/git/app")));
    if (fresh.repo() != QLatin1String("/home/me/git/app") || fresh.value() != QLatin1String("x")) {
        return 11;
    }

    SecretDialog existing(QStringLiteral("API_KEY"), QStringLiteral("/home/me/git/app"), {});
    auto* fixedName = existing.findChild<QLineEdit*>(QStringLiteral("SecretName"));
    auto* fixedScope = existing.findChild<QComboBox*>(QStringLiteral("SecretScope"));
    if (fixedName == nullptr || !fixedName->isReadOnly() || fixedScope == nullptr ||
        fixedScope->isEnabled() || existing.repo() != QLatin1String("/home/me/git/app")) {
        return 12;
    }
    return 0;
}
#endif
