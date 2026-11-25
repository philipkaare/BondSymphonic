#include "HostSetupDialog.h"
#include "bondsymphonic-ide/src/qobjects/host_setup.cxxqt.h"
#include <QFontDatabase>
#include <QHBoxLayout>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPlainTextEdit>
#include <QProgressBar>
#include <QPushButton>
#include <QTextDocument>
#include <QVBoxLayout>

HostSetupDialog::HostSetupDialog(HostSetup* setup, QWidget* parent)
    : QDialog(parent), m_setup(setup) {
    setObjectName(QStringLiteral("HostSetupDialog"));
    setWindowTitle(QStringLiteral("Setting up BondSymphonic"));
    setModal(false);
    resize(720, 460);

    auto* layout = new QVBoxLayout(this);
    m_message = new QLabel(this);
    m_message->setObjectName(QStringLiteral("HostSetupMessage"));
    m_message->setWordWrap(true);
    m_message->setTextFormat(Qt::PlainText);
    layout->addWidget(m_message);

    // Indeterminate: the scripts print what they are doing, not how far along
    // they are, and a bar that guesses is worse than one that says "busy".
    m_busy = new QProgressBar(this);
    m_busy->setRange(0, 0);
    m_busy->setTextVisible(false);
    layout->addWidget(m_busy);

    m_log = new QPlainTextEdit(this);
    m_log->setObjectName(QStringLiteral("HostSetupLog"));
    m_log->setReadOnly(true);
    m_log->setMaximumBlockCount(5000);
    m_log->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    layout->addWidget(m_log, 1);

    auto* buttons = new QHBoxLayout;
    m_installWsl = new QPushButton(QStringLiteral("Install WSL…"), this);
    m_retry = new QPushButton(QStringLiteral("Try again"), this);
    m_skip = new QPushButton(QStringLiteral("Continue without it"), this);
    m_skip->setToolTip(QStringLiteral(
        "Start BondSymphonic anyway. Agents cannot run until the Linux environment is set up."));
    m_close = new QPushButton(QStringLiteral("Hide"), this);
    buttons->addWidget(m_installWsl);
    buttons->addStretch(1);
    buttons->addWidget(m_retry);
    buttons->addWidget(m_skip);
    buttons->addWidget(m_close);
    layout->addLayout(buttons);

    connect(m_installWsl, &QPushButton::clicked, this, [this] {
        m_log->clear();
        m_setup->installWsl();
    });
    connect(m_retry, &QPushButton::clicked, this, [this] {
        m_log->clear();
        m_setup->start();
    });
    connect(m_skip, &QPushButton::clicked, this, [this] {
        m_setup->skip();
        hide();
    });
    connect(m_close, &QPushButton::clicked, this, &QDialog::hide);
    connect(m_setup, &HostSetup::statusChanged, this, [this] { refresh(); });
    connect(m_setup, &HostSetup::output, this,
            [this](const QString& line) { m_log->appendPlainText(line); });
    refresh();
}

void HostSetupDialog::refresh() {
    const auto status = QJsonDocument::fromJson(m_setup->statusJson().toUtf8()).object();
    const QString phase = status.value(QStringLiteral("phase")).toString();
    const QString message = status.value(QStringLiteral("message")).toString();

    if (phase == QLatin1String("idle") || phase == QLatin1String("checking")) {
        return;
    }
    if (phase == QLatin1String("ready")) {
        hide();
        return;
    }
    const bool working = phase == QLatin1String("working");
    const bool needsWsl = phase == QLatin1String("needs_wsl");
    const bool failed = phase == QLatin1String("failed");
    const bool needsRestart = phase == QLatin1String("needs_restart");

    m_message->setText(failed ? QStringLiteral("The setup did not finish: %1").arg(message)
                              : message);
    m_busy->setVisible(working);
    m_log->setVisible(working || failed || !m_log->document()->isEmpty());
    m_installWsl->setVisible(needsWsl);
    m_retry->setVisible(failed);
    m_skip->setVisible(needsWsl || failed);
    m_close->setText(needsRestart ? QStringLiteral("Close") : QStringLiteral("Hide"));
    if (!isVisible()) {
        show();
    }
    raise();
}
