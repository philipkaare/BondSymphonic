#include "NetworkBar.h"
#include "CodeView.h"
#include "Theme.h"
#include <QDateTime>
#include <QEvent>
#include <QHBoxLayout>
#include <QJsonObject>
#include <QLabel>
#include <QPalette>
#include <QPushButton>
#include <QSizePolicy>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>
#if defined(BS_WIDGET_TESTS)
#include <QCoreApplication>
#include <cstdint>
#endif

namespace {

constexpr QChar kTimes(0x00D7);
constexpr QChar kEllipsis(0x2026);

/// "1:05" for 65 seconds: a countdown read at a glance.
QString clock(qint64 seconds) {
    const qint64 s = seconds < 0 ? 0 : seconds;
    return QStringLiteral("%1:%2").arg(s / 60).arg(s % 60, 2, 10, QLatin1Char('0'));
}

} // namespace

NetworkBar::NetworkBar(QWidget* parent) : QFrame(parent) {
    setObjectName(QStringLiteral("TranscriptNetworkBar"));
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    applyWash();
    m_rows = new QVBoxLayout(this);
    m_rows->setContentsMargins(6, 4, 6, 4);
    m_rows->setSpacing(4);
    m_tick = new QTimer(this);
    m_tick->setInterval(1000);
    QObject::connect(m_tick, &QTimer::timeout, this, &NetworkBar::tick);
    hide();
}

void NetworkBar::setRequests(const QJsonArray& rows) {
    m_requests = rows;
    rebuild();
    if (m_requests.isEmpty()) {
        m_tick->stop();
        hide();
    } else {
        if (!m_tick->isActive()) {
            m_tick->start();
        }
        show();
    }
}

QString NetworkBar::rowText(const QJsonObject& row, qint64 nowMs) {
    const QString host = row.value(QStringLiteral("host")).toString();
    const QString status = row.value(QStringLiteral("status")).toString();
    const bool allowable = row.value(QStringLiteral("allowable")).toBool();
    const int count = row.value(QStringLiteral("count")).toInt();
    const qint64 left = (static_cast<qint64>(row.value(QStringLiteral("until_ms")).toDouble()) -
                         nowMs + 999) /
                        1000;
    QString text;
    if (status == QLatin1String("allowed")) {
        text = QStringLiteral("Allowed %1 for this workspace: the waiting request goes through.")
                   .arg(host);
    } else if (status == QLatin1String("always_blocked")) {
        text = QStringLiteral("%1 is always blocked for this workspace. Change it in Workspace → "
                              "Network rules…")
                   .arg(host);
    } else if (status == QLatin1String("allowing")) {
        text = QStringLiteral("Saving the answer for %1").arg(host) + kEllipsis;
    } else if (allowable) {
        text = QStringLiteral("The agent wants to reach %1, which is not on this workspace's "
                              "allowlist. It is waiting for you: %2 left.")
                   .arg(host, clock(left));
    } else {
        // A refused model-server tunnel, in the daemon's own words.
        text = row.value(QStringLiteral("text")).toString();
    }
    if (count > 1 && status != QLatin1String("allowed") &&
        status != QLatin1String("always_blocked")) {
        text += QStringLiteral(" (") + kTimes + QString::number(count) + QStringLiteral(")");
    }
    if (status == QLatin1String("failed")) {
        text += QStringLiteral("\nCould not save the answer: %1")
                    .arg(row.value(QStringLiteral("error")).toString());
    }
    return text;
}

void NetworkBar::rebuild() {
    while (QLayoutItem* item = m_rows->takeAt(0)) {
        if (QWidget* w = item->widget()) {
            w->deleteLater();
        }
        delete item;
    }
    const qint64 now = QDateTime::currentMSecsSinceEpoch();
    for (const QJsonValue& value : m_requests) {
        const QJsonObject row = value.toObject();
        const QString host = row.value(QStringLiteral("host")).toString();
        const QString status = row.value(QStringLiteral("status")).toString();
        const bool allowable = row.value(QStringLiteral("allowable")).toBool();
        const bool settled =
            status == QLatin1String("allowed") || status == QLatin1String("always_blocked");
        const bool saving = status == QLatin1String("allowing");

        auto* line = new QWidget(this);
        line->setObjectName(QStringLiteral("NetworkBarRow"));
        line->setProperty("host", host);
        auto* layout = new QHBoxLayout(line);
        layout->setContentsMargins(0, 0, 0, 0);
        layout->setSpacing(8);
        auto* label = new QLabel(rowText(row, now), line);
        label->setObjectName(QStringLiteral("NetworkBarText"));
        label->setTextFormat(Qt::PlainText);
        label->setWordWrap(true);
        label->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
        label->setTextInteractionFlags(Qt::TextSelectableByMouse);
        layout->addWidget(label, 1);
        if (!settled) {
            if (allowable) {
                auto* allow = new QPushButton(status == QLatin1String("failed")
                                                  ? QStringLiteral("Try again")
                                                  : QStringLiteral("Allow"),
                                              line);
                allow->setObjectName(QStringLiteral("NetworkBarAllow"));
                allow->setToolTip(QStringLiteral("Add %1 to this workspace's network allowlist; "
                                                 "the waiting request goes through")
                                      .arg(host));
                allow->setEnabled(!saving);
                layout->addWidget(allow, 0, Qt::AlignTop);
                QObject::connect(allow, &QPushButton::clicked, this,
                                 [this, host] { Q_EMIT allowRequested(host); });
            }
            auto* block = new QPushButton(QStringLiteral("Always block"), line);
            block->setObjectName(QStringLiteral("NetworkBarBlock"));
            block->setToolTip(QStringLiteral("Always block %1 for this workspace; the waiting "
                                             "request is refused now")
                                  .arg(host));
            block->setEnabled(!saving);
            layout->addWidget(block, 0, Qt::AlignTop);
            QObject::connect(block, &QPushButton::clicked, this,
                             [this, host] { Q_EMIT blockRequested(host); });
        }
        auto* dismiss = new QToolButton(line);
        dismiss->setObjectName(QStringLiteral("NetworkBarDismiss"));
        dismiss->setText(QString(kTimes));
        dismiss->setAutoRaise(true);
        dismiss->setToolTip(settled ? QStringLiteral("Close")
                                    : QStringLiteral("Stop asking; the request is refused when "
                                                     "its time runs out"));
        dismiss->setEnabled(!saving);
        layout->addWidget(dismiss, 0, Qt::AlignTop);
        QObject::connect(dismiss, &QToolButton::clicked, this,
                         [this, host] { Q_EMIT dismissRequested(host); });
        m_rows->addWidget(line);
    }
}

void NetworkBar::tick() {
    // The countdowns, redrawn in place: rebuilding would take the focus off a
    // button somebody is tabbing to.
    const qint64 now = QDateTime::currentMSecsSinceEpoch();
    int i = 0;
    for (const QJsonValue& value : m_requests) {
        QLayoutItem* item = m_rows->itemAt(i++);
        QWidget* line = item == nullptr ? nullptr : item->widget();
        auto* label = line == nullptr ? nullptr
                                      : line->findChild<QLabel*>(QStringLiteral("NetworkBarText"));
        if (label != nullptr) {
            label->setText(rowText(value.toObject(), now));
        }
    }
    Q_EMIT expireRequested();
}

void NetworkBar::applyWash() {
    if (m_mixing) {
        return;
    }
    m_mixing = true;
    setPalette(QPalette());
    QPalette barPalette;
    barPalette.setColor(QPalette::Window, codeview::wash(palette().base().color(),
                                                        theme::changed(), codeview::kWashAmount));
    setPalette(barPalette);
    m_mixing = false;
}

void NetworkBar::changeEvent(QEvent* event) {
    QFrame::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyWash();
    }
}

#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

/// The bar shows one row per waiting host with its countdown and both answers,
/// says what became of an answered one without offering them again, sends the
/// host with every button, and goes away when there is nothing left.
extern "C" std::int32_t bs_widget_test_network_bar_lists_waiting_requests() {
    QWidget host;
    auto* bar = new NetworkBar(&host);
    host.show();
    QStringList allowed;
    QStringList blocked;
    QStringList dismissed;
    QObject::connect(bar, &NetworkBar::allowRequested, &host,
                     [&allowed](const QString& h) { allowed.append(h); });
    QObject::connect(bar, &NetworkBar::blockRequested, &host,
                     [&blocked](const QString& h) { blocked.append(h); });
    QObject::connect(bar, &NetworkBar::dismissRequested, &host,
                     [&dismissed](const QString& h) { dismissed.append(h); });
    const double until = static_cast<double>(QDateTime::currentMSecsSinceEpoch() + 65'000);
    QJsonArray rows;
    rows.append(QJsonObject{ { "host", "pypi.org" },
                             { "count", 2 },
                             { "allowable", true },
                             { "text", "" },
                             { "status", "blocked" },
                             { "error", "" },
                             { "until_ms", until } });
    rows.append(QJsonObject{ { "host", "done.example" },
                             { "count", 1 },
                             { "allowable", true },
                             { "text", "" },
                             { "status", "allowed" },
                             { "error", "" },
                             { "until_ms", until } });
    bar->setRequests(rows);
    if (!bar->isVisible()) {
        return 1;
    }
    const auto lines = bar->findChildren<QWidget*>(QStringLiteral("NetworkBarRow"));
    if (lines.size() != 2) {
        return 2;
    }
    QWidget* waiting = lines.value(0);
    auto* text = waiting->findChild<QLabel*>(QStringLiteral("NetworkBarText"));
    if (text == nullptr || !text->text().contains(QStringLiteral("pypi.org")) ||
        !text->text().contains(QStringLiteral("1:0")) ||
        !text->text().contains(QStringLiteral("waiting for you"))) {
        return 3;
    }
    auto* allow = waiting->findChild<QPushButton*>(QStringLiteral("NetworkBarAllow"));
    auto* block = waiting->findChild<QPushButton*>(QStringLiteral("NetworkBarBlock"));
    if (allow == nullptr || block == nullptr) {
        return 4;
    }
    allow->click();
    block->click();
    if (allowed != QStringList{ "pypi.org" } || blocked != QStringList{ "pypi.org" }) {
        return 5;
    }
    // The answered row offers neither answer again, only a close.
    QWidget* done = lines.value(1);
    if (done->findChild<QPushButton*>(QStringLiteral("NetworkBarAllow")) != nullptr ||
        done->findChild<QPushButton*>(QStringLiteral("NetworkBarBlock")) != nullptr) {
        return 6;
    }
    auto* close = done->findChild<QToolButton*>(QStringLiteral("NetworkBarDismiss"));
    if (close == nullptr) {
        return 7;
    }
    close->click();
    if (dismissed != QStringList{ "done.example" }) {
        return 8;
    }
    bar->setRequests(QJsonArray());
    if (bar->isVisible()) {
        return 9;
    }
    return 0;
}
#endif
