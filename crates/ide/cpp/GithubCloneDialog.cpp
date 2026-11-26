#include "GithubCloneDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
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
#include <QListWidget>
#include <QPushButton>
#include <QRegularExpression>
#include <QVBoxLayout>
#include <algorithm>
#if defined(BS_WIDGET_TESTS)
#include <cstdint>
#endif

namespace {
constexpr QChar kEllipsis(0x2026);
} // namespace

GithubCloneDialog::GithubCloneDialog(AppController* controller, const QString& parentDir,
                                     QWidget* parent)
    : QDialog(parent), m_controller(controller) {
    setWindowTitle(QStringLiteral("Clone from GitHub"));
    setModal(true);
    resize(560, 460);
    auto* outer = new QVBoxLayout(this);

    m_filter = new QLineEdit(this);
    m_filter->setObjectName(QStringLiteral("GithubCloneFilter"));
    m_filter->setPlaceholderText(QStringLiteral("Filter your repositories"));
    m_filter->setClearButtonEnabled(true);
    outer->addWidget(m_filter);

    m_list = new QListWidget(this);
    m_list->setObjectName(QStringLiteral("GithubCloneList"));
    outer->addWidget(m_list, 1);

    auto* form = new QFormLayout();
    m_repo = new QLineEdit(this);
    m_repo->setObjectName(QStringLiteral("GithubCloneRepo"));
    m_repo->setPlaceholderText(QStringLiteral("owner/name or https://github.com/owner/name"));
    form->addRow(QStringLiteral("Repository:"), m_repo);

    auto* parentRow = new QHBoxLayout();
    m_parentDir = new QLineEdit(parentDir, this);
    m_parentDir->setObjectName(QStringLiteral("GithubCloneParent"));
    m_parentDir->setToolTip(
        QStringLiteral("The repository is cloned into a new folder, named after it, in here"));
    m_browse = new QPushButton(QStringLiteral("Browse") + kEllipsis, this);
    parentRow->addWidget(m_parentDir, 1);
    parentRow->addWidget(m_browse, 0);
    form->addRow(QStringLiteral("Clone into:"), parentRow);
    outer->addLayout(form);

    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("GithubCloneStatus"));
    m_status->setWordWrap(true);
    // The daemon's and gh's words, with paths and repository names in them.
    m_status->setTextFormat(Qt::PlainText);
    m_status->setTextInteractionFlags(Qt::TextSelectableByMouse);
    outer->addWidget(m_status);

    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Clone"));
    outer->addWidget(m_buttons);

    QObject::connect(m_filter, &QLineEdit::textChanged, this, [this] { applyFilter(); });
    QObject::connect(m_list, &QListWidget::currentItemChanged, this,
                     [this](QListWidgetItem* item) {
                         if (item != nullptr) {
                             m_repo->setText(item->data(Qt::UserRole).toString());
                         }
                     });
    QObject::connect(m_list, &QListWidget::itemDoubleClicked, this, [this] { startClone(); });
    QObject::connect(m_repo, &QLineEdit::textChanged, this, [this] { updateEnabled(); });
    QObject::connect(m_parentDir, &QLineEdit::textChanged, this, [this] { updateEnabled(); });
    QObject::connect(m_browse, &QPushButton::clicked, this, [this] { browse(); });
    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, [this] { startClone(); });
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    QObject::connect(m_controller, &AppController::githubReposListed, this,
                     [this](const QString& json) { onListed(json); });
    QObject::connect(m_controller, &AppController::githubReposFailed, this,
                     [this](const QString& message) { onListFailed(message); });
    QObject::connect(m_controller, &AppController::githubRepoCloned, this,
                     [this](const QString& repo, const QString& path) {
                         if (repo != m_cloning) {
                             return;
                         }
                         m_cloning.clear();
                         m_clonedPath = path;
                         accept();
                     });
    QObject::connect(m_controller, &AppController::githubRepoCloneFailed, this,
                     [this](const QString& repo, const QString& message) {
                         if (repo != m_cloning) {
                             return;
                         }
                         m_cloning.clear();
                         setBusy(false);
                         m_status->setText(QStringLiteral("Could not clone %1: %2").arg(repo, message));
                     });

    m_status->setText(QStringLiteral("Loading your GitHub repositories") + kEllipsis);
    updateEnabled();
    m_controller->listGithubRepos();
}

QString GithubCloneDialog::targetName(const QString& repo) {
    QString s = repo.trimmed();
    while (s.endsWith(QLatin1Char('/'))) {
        s.chop(1);
    }
    if (s.endsWith(QStringLiteral(".git"))) {
        s.chop(4);
    }
    const qsizetype cut = std::max(s.lastIndexOf(QLatin1Char('/')), s.lastIndexOf(QLatin1Char(':')));
    if (cut < 0) {
        // A bare name is no repository: the owner is part of it.
        return QString();
    }
    const QString name = s.mid(cut + 1);
    static const QRegularExpression valid(QStringLiteral("^[A-Za-z0-9._-]+$"));
    if (name.isEmpty() || name == QLatin1String(".") || name == QLatin1String("..") ||
        !valid.match(name).hasMatch()) {
        return QString();
    }
    return name;
}

void GithubCloneDialog::onListed(const QString& json) {
    m_list->clear();
    const QJsonArray repos = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& value : repos) {
        const QJsonObject repo = value.toObject();
        const QString slug = repo.value(QStringLiteral("name_with_owner")).toString();
        if (slug.isEmpty()) {
            continue;
        }
        const QString description = repo.value(QStringLiteral("description")).toString();
        const bool isPrivate = repo.value(QStringLiteral("private")).toBool();
        auto* item = new QListWidgetItem(
            isPrivate ? slug + QStringLiteral("  (private)") : slug, m_list);
        item->setData(Qt::UserRole, slug);
        if (!description.isEmpty()) {
            item->setToolTip(description);
        }
    }
    m_status->setText(repos.isEmpty()
                          ? QStringLiteral("No repositories found for your GitHub account. You "
                                           "can still type owner/name or a URL.")
                          : QString());
    applyFilter();
}

void GithubCloneDialog::onListFailed(const QString& message) {
    m_status->setText(QStringLiteral("Could not list your GitHub repositories: %1\nYou can still "
                                     "type owner/name or a URL.")
                          .arg(message));
}

void GithubCloneDialog::applyFilter() {
    const QString needle = m_filter->text().trimmed();
    for (int i = 0; i < m_list->count(); ++i) {
        QListWidgetItem* item = m_list->item(i);
        item->setHidden(!needle.isEmpty() &&
                        !item->text().contains(needle, Qt::CaseInsensitive) &&
                        !item->toolTip().contains(needle, Qt::CaseInsensitive));
    }
}

void GithubCloneDialog::browse() {
    const QString dir = QFileDialog::getExistingDirectory(
        this, QStringLiteral("Clone into which folder?"), m_parentDir->text());
    if (!dir.isEmpty()) {
        m_parentDir->setText(QDir::toNativeSeparators(dir));
    }
}

void GithubCloneDialog::startClone() {
    if (!m_cloning.isEmpty()) {
        return;
    }
    const QString repo = m_repo->text().trimmed();
    const QString name = targetName(repo);
    if (name.isEmpty()) {
        m_status->setText(QStringLiteral("Name the repository as owner/name or a GitHub URL."));
        return;
    }
    const QString parentDir = m_controller->wslPath(m_parentDir->text().trimmed());
    if (parentDir.isEmpty()) {
        m_status->setText(
            QStringLiteral("%1 is not a folder the daemon can reach.").arg(m_parentDir->text()));
        return;
    }
    m_cloning = repo;
    setBusy(true);
    m_status->setText(QStringLiteral("Cloning %1 into %2/%3").arg(repo, parentDir, name) +
                      kEllipsis);
    m_controller->cloneGithubRepo(repo, parentDir);
}

void GithubCloneDialog::setBusy(bool busy) {
    m_filter->setEnabled(!busy);
    m_list->setEnabled(!busy);
    m_repo->setEnabled(!busy);
    m_parentDir->setEnabled(!busy);
    m_browse->setEnabled(!busy);
    updateEnabled();
}

void GithubCloneDialog::updateEnabled() {
    m_buttons->button(QDialogButtonBox::Ok)
        ->setEnabled(m_cloning.isEmpty() && !targetName(m_repo->text()).isEmpty() &&
                     !m_parentDir->text().trimmed().isEmpty());
}

#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

/// The list fills from the controller's answer and filters on the name and the
/// description; picking a row names it; a clone goes to the daemon with the
/// folder, blocks a second one, and the answer for it accepts the dialog with
/// the path the daemon gave.
extern "C" std::int32_t bs_widget_test_github_clone_dialog_clones_the_chosen_repo() {
    if (GithubCloneDialog::targetName(QStringLiteral("octo/hello.git")) != QLatin1String("hello") ||
        GithubCloneDialog::targetName(QStringLiteral("https://github.com/octo/hello/")) !=
            QLatin1String("hello") ||
        GithubCloneDialog::targetName(QStringLiteral("git@github.com:octo/hello.git")) !=
            QLatin1String("hello") ||
        !GithubCloneDialog::targetName(QStringLiteral("hello")).isEmpty() ||
        !GithubCloneDialog::targetName(QStringLiteral("octo/..")).isEmpty()) {
        return 1;
    }
    AppController controller;
    GithubCloneDialog dialog(&controller, QStringLiteral("/home/me/git"));
    auto* list = dialog.findChild<QListWidget*>(QStringLiteral("GithubCloneList"));
    auto* filter = dialog.findChild<QLineEdit*>(QStringLiteral("GithubCloneFilter"));
    auto* repo = dialog.findChild<QLineEdit*>(QStringLiteral("GithubCloneRepo"));
    auto* status = dialog.findChild<QLabel*>(QStringLiteral("GithubCloneStatus"));
    auto* buttons = dialog.findChild<QDialogButtonBox*>();
    if (list == nullptr || filter == nullptr || repo == nullptr || status == nullptr ||
        buttons == nullptr) {
        return 2;
    }
    QPushButton* clone = buttons->button(QDialogButtonBox::Ok);
    if (clone->isEnabled()) {
        return 3;
    }
    controller.githubReposListed(QStringLiteral(
        R"([{"name_with_owner":"octo/hello","description":"Says hi","private":false},)"
        R"({"name_with_owner":"octo/secret","description":null,"private":true}])"));
    if (list->count() != 2 || !list->item(1)->text().contains(QStringLiteral("private"))) {
        return 4;
    }
    filter->setText(QStringLiteral("says"));
    if (list->item(0)->isHidden() || !list->item(1)->isHidden()) {
        return 5;
    }
    filter->clear();
    list->setCurrentRow(1);
    if (repo->text() != QLatin1String("octo/secret") || !clone->isEnabled()) {
        return 6;
    }
    clone->click();
    if (clone->isEnabled() || repo->isEnabled() ||
        !status->text().contains(QStringLiteral("/home/me/git/secret"))) {
        return 7;
    }
    // Another repository's answer is not this dialog's.
    controller.githubRepoCloned(QStringLiteral("octo/hello"), QStringLiteral("/x/hello"));
    if (dialog.result() == QDialog::Accepted || !dialog.clonedPath().isEmpty()) {
        return 8;
    }
    controller.githubRepoCloneFailed(QStringLiteral("octo/secret"), QStringLiteral("exists"));
    if (!clone->isEnabled() || !status->text().contains(QStringLiteral("exists"))) {
        return 9;
    }
    clone->click();
    controller.githubRepoCloned(QStringLiteral("octo/secret"),
                                QStringLiteral("/home/me/git/secret"));
    if (dialog.result() != QDialog::Accepted ||
        dialog.clonedPath() != QLatin1String("/home/me/git/secret")) {
        return 10;
    }
    return 0;
}
#endif
