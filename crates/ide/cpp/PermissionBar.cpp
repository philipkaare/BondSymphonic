#include "PermissionBar.h"
#include "CodeView.h"
#include "Theme.h"
#include <QCheckBox>
#include <QColor>
#include <QFont>
#include <QHBoxLayout>
#include <QLabel>
#include <QPalette>
#include <QPushButton>
#include <QSizePolicy>

PermissionBar::PermissionBar(QWidget* parent) : QFrame(parent) {
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    QPalette barPalette = palette();
    // Amber: the bar is asking, not reporting a failure, and "changed" is the
    // one accent the theme reserves for something waiting on the user.
    barPalette.setColor(QPalette::Window,
                        codeview::wash(palette().base().color(), theme::changed(),
                                       codeview::kWashAmount));
    setPalette(barPalette);

    auto* layout = new QHBoxLayout(this);
    layout->setContentsMargins(6, 4, 6, 4);
    layout->setSpacing(8);

    m_prompt = new QLabel(this);
    m_prompt->setTextFormat(Qt::PlainText);
    m_prompt->setWordWrap(true);
    m_prompt->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    layout->addWidget(m_prompt, 1);

    m_always = new QCheckBox(QStringLiteral("Always allow this tool for this session"), this);
    layout->addWidget(m_always, 0);

    auto* allow = new QPushButton(QStringLiteral("Allow"), this);
    allow->setDefault(true);
    auto* deny = new QPushButton(QStringLiteral("Deny"), this);
    layout->addWidget(allow, 0);
    layout->addWidget(deny, 0);

    QObject::connect(allow, &QPushButton::clicked, this,
                     [this] { Q_EMIT allowed(m_always->isChecked()); });
    QObject::connect(deny, &QPushButton::clicked, this, [this] { Q_EMIT denied(); });

    hide();
}

void PermissionBar::show(const QJsonObject& pending) {
    const QString requestId = pending.value(QStringLiteral("request_id")).toString();
    const QString tool = pending.value(QStringLiteral("tool_name")).toString();
    const QString summary = pending.value(QStringLiteral("summary")).toString();
    if (requestId != m_requestId) {
        // A different request: whatever was ticked was ticked about the last
        // one, and carrying it over would allowlist a tool nobody agreed to.
        m_always->setChecked(false);
        m_requestId = requestId;
    }
    m_prompt->setText(summary.isEmpty() || summary == tool
                          ? QStringLiteral("Allow %1?").arg(tool)
                          : QStringLiteral("Allow %1: %2?").arg(tool, summary));
    m_prompt->setToolTip(pending.value(QStringLiteral("input_json")).toString());
    QFrame::show();
}

void PermissionBar::clear() {
    m_requestId.clear();
    m_always->setChecked(false);
    m_prompt->clear();
    m_prompt->setToolTip(QString());
    hide();
}

QString PermissionBar::requestId() const { return m_requestId; }
