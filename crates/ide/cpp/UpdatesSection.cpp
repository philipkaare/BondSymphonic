#include "UpdatesSection.h"
#include "bondsymphonic-ide/src/qobjects/update_controller.cxxqt.h"
#include <QCheckBox>
#include <QDateTime>
#include <QHBoxLayout>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLocale>
#include <QProgressBar>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

const QString kReleasesUrl =
    QStringLiteral("https://github.com/philipkaare/BondSymphonic/releases");

} // namespace

UpdatesSection::UpdatesSection(UpdateController* updates, QWidget* parent)
    : QGroupBox(QStringLiteral("Updates"), parent), m_updates(updates) {
    setObjectName(QStringLiteral("SettingsUpdates"));
    auto* layout = new QVBoxLayout(this);

    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("UpdatesStatus"));
    m_status->setWordWrap(true);
    m_status->setTextFormat(Qt::RichText);
    m_status->setOpenExternalLinks(true);
    layout->addWidget(m_status);

    m_progress = new QProgressBar(this);
    m_progress->setRange(0, 100);
    m_progress->setVisible(false);
    layout->addWidget(m_progress);

    m_automatic = new QCheckBox(QStringLiteral("Download and install updates automatically"), this);
    m_automatic->setObjectName(QStringLiteral("UpdatesAutomatic"));
    m_automatic->setToolTip(QStringLiteral(
        "On: BondSymphonic checks for a new version when it starts and every few hours, downloads "
        "it in the background, and installs it when you quit.\n"
        "Off: nothing is checked or downloaded until you click Check now."));
    layout->addWidget(m_automatic);

    auto* row = new QHBoxLayout;
    m_check = new QPushButton(QStringLiteral("Check now"), this);
    m_check->setObjectName(QStringLiteral("UpdatesCheckNow"));
    m_restart = new QPushButton(QStringLiteral("Restart to update"), this);
    m_restart->setObjectName(QStringLiteral("UpdatesRestart"));
    m_restart->setVisible(false);
    row->addWidget(m_check);
    row->addWidget(m_restart);
    row->addStretch(1);
    layout->addLayout(row);

    if (m_updates == nullptr) {
        m_status->setText(QStringLiteral("Updates are not available in this build."));
        m_automatic->setEnabled(false);
        m_check->setEnabled(false);
        return;
    }
    m_automatic->setChecked(m_updates->automatic());
    connect(m_check, &QPushButton::clicked, this, [this] { m_updates->checkNow(); });
    connect(m_restart, &QPushButton::clicked, this, &UpdatesSection::restartClicked);
    connect(m_updates, &UpdateController::statusChanged, this, [this] { refresh(); });
    refresh();
}

void UpdatesSection::apply() {
    if (m_updates != nullptr) {
        m_updates->setAutomatic(m_automatic->isChecked());
    }
}

void UpdatesSection::refresh() {
    const auto s = QJsonDocument::fromJson(m_updates->statusJson().toUtf8()).object();
    const QString phase = s.value(QStringLiteral("phase")).toString();
    const QString current = s.value(QStringLiteral("current")).toString();
    const QString latest = s.value(QStringLiteral("latest")).toString();
    const QString message = s.value(QStringLiteral("message")).toString().toHtmlEscaped();
    QString notes = s.value(QStringLiteral("notes_url")).toString();
    if (notes.isEmpty()) {
        notes = kReleasesUrl;
    }
    const qint64 checked = static_cast<qint64>(s.value(QStringLiteral("last_checked")).toDouble());

    QString text;
    if (phase == QLatin1String("checking")) {
        text = QStringLiteral("Checking for updates…");
    } else if (phase == QLatin1String("downloading")) {
        text = QStringLiteral("Downloading version %1…").arg(latest);
    } else if (phase == QLatin1String("ready")) {
        text = QStringLiteral("Version %1 is downloaded and installs when you quit BondSymphonic. "
                              "<a href=\"%2\">What's new</a>")
                   .arg(latest, notes);
    } else if (phase == QLatin1String("up_to_date")) {
        text = QStringLiteral("BondSymphonic %1 is up to date.").arg(current);
    } else if (phase == QLatin1String("available")) {
        text = QStringLiteral("Version %1 is available. %2 <a href=\"%3\">Releases</a>")
                   .arg(latest, message, kReleasesUrl);
    } else if (phase == QLatin1String("failed")) {
        text = QStringLiteral("BondSymphonic %1. The last update check failed: %2")
                   .arg(current, message);
    } else if (phase == QLatin1String("unavailable")) {
        text = QStringLiteral("BondSymphonic %1. Updates are not available for this build: %2")
                   .arg(current, message);
    } else {
        text = QStringLiteral("BondSymphonic %1.").arg(current);
    }
    if (checked > 0 && phase != QLatin1String("checking") && phase != QLatin1String("downloading")) {
        text += QStringLiteral(" <span style=\"color: gray\">Last checked %1.</span>")
                    .arg(QLocale().toString(QDateTime::fromSecsSinceEpoch(checked),
                                            QLocale::ShortFormat));
    }
    m_status->setText(text);

    const bool busy = phase == QLatin1String("checking") || phase == QLatin1String("downloading");
    m_progress->setVisible(phase == QLatin1String("downloading"));
    m_progress->setValue(static_cast<int>(s.value(QStringLiteral("progress")).toDouble() * 100));
    m_check->setEnabled(!busy);
    m_restart->setVisible(phase == QLatin1String("ready"));
}
