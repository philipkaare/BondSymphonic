#include "TranscriptView.h"
#include "CodeView.h"
#include "PermissionBar.h"
#include "PromptInput.h"
#include "Theme.h"
#include "ToolCard.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QChar>
#include <QFont>
#include <QFontMetrics>
#include <QFrame>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLabel>
#include <QPalette>
#include <QPushButton>
#include <QScrollArea>
#include <QScrollBar>
#include <QVBoxLayout>
#include <QVariant>

namespace {

/// The property a text frame records its item kind in, so a frame is only
/// updated in place while it is still the right shape for the item.
const char* kKindProperty = "bsTranscriptKind";

/// The object name of the label inside a text frame.
const char* kBodyName = "bsTranscriptBody";

/// MIDDLE DOT, the separator in a result line. A code point rather than a
/// literal: the sources carry no BOM, so a compiler reading them in the
/// system code page would mangle it.
constexpr char16_t kMiddleDot = 0x00B7;

/// How much smaller than the pane's font the result and system lines are.
constexpr qreal kSmallTextScale = 0.85;

/// The daemon's state words this view reacts to. Anything else is a state it
/// has nothing to say about, which is the right answer for a word added later.
const char* kStateWorking = "working";
const char* kStateError = "error";
const char* kStateExited = "exited";

/// The label inside a text frame, or null for a frame built some other way.
QLabel* bodyLabel(QWidget* frame) {
    return frame == nullptr ? nullptr : frame->findChild<QLabel*>(QString::fromUtf8(kBodyName));
}

/// A frame holding one label, tagged with the item kind it was built for.
QFrame* makeTextFrame(const QString& kind, QWidget* parent) {
    auto* frame = new QFrame(parent);
    frame->setProperty(kKindProperty, kind);
    frame->setFrameShape(QFrame::NoFrame);
    auto* layout = new QVBoxLayout(frame);
    layout->setContentsMargins(6, 2, 6, 2);
    layout->setSpacing(0);
    auto* label = new QLabel(frame);
    label->setObjectName(QString::fromUtf8(kBodyName));
    label->setWordWrap(true);
    // Selectable but not focusable: dragging over an answer to copy it must
    // work, and Tab must still land in the prompt box rather than in the
    // transcript.
    label->setTextInteractionFlags(Qt::TextSelectableByMouse);
    layout->addWidget(label);
    return frame;
}

/// The one grey the small lines share.
void applySmallGrey(QLabel* label, bool italic) {
    QFont font = label->font();
    font.setPointSizeF(font.pointSizeF() * kSmallTextScale);
    font.setItalic(italic);
    label->setFont(font);
    label->setEnabled(false);
}

/// "turn 3 . $0.0042 . 1.2 s" for one result item, with a middle dot for the
/// separator.
QString resultLine(const QJsonObject& item) {
    const double cost = item.value(QStringLiteral("cost_usd")).toDouble();
    const double seconds = item.value(QStringLiteral("duration_ms")).toDouble() / 1000.0;
    const int turns = item.value(QStringLiteral("num_turns")).toInt();
    const QString dot = QStringLiteral(" ") + QChar(kMiddleDot) + QStringLiteral(" ");
    return QStringLiteral("turn %1").arg(turns) + dot +
           QStringLiteral("$%1").arg(cost, 0, 'f', 4) + dot +
           QStringLiteral("%1 s").arg(seconds, 0, 'f', 1);
}

/// What a text frame's label says for `item`. The result line is composed;
/// every other kind carries its own text.
QString bodyText(const QJsonObject& item) {
    if (item.value(QStringLiteral("kind")).toString() == QStringLiteral("result")) {
        return resultLine(item);
    }
    return item.value(QStringLiteral("text")).toString();
}

} // namespace

TranscriptView::TranscriptView(TranscriptModel* model, QWidget* parent)
    : QWidget(parent), m_model(model) {
    auto* outer = new QVBoxLayout(this);
    outer->setContentsMargins(0, 0, 0, 0);
    outer->setSpacing(0);

    m_scroll = new QScrollArea(this);
    m_scroll->setWidgetResizable(true);
    m_scroll->setFrameShape(QFrame::NoFrame);
    auto* column = new QWidget(m_scroll);
    m_frameLayout = new QVBoxLayout(column);
    m_frameLayout->setContentsMargins(0, 4, 0, 4);
    m_frameLayout->setSpacing(4);
    // The stretch is the column's last entry for the life of the view; every
    // frame is inserted in front of it, so a short conversation sits at the top
    // instead of being spread down the pane.
    m_frameLayout->addStretch(1);
    m_scroll->setWidget(column);
    outer->addWidget(m_scroll, 1);

    m_banner = new QLabel(this);
    m_banner->setTextFormat(Qt::PlainText);
    m_banner->setWordWrap(true);
    m_banner->setMargin(4);
    m_banner->setAutoFillBackground(true);
    QPalette bannerPalette = m_banner->palette();
    bannerPalette.setColor(QPalette::Window,
                           codeview::wash(palette().base().color(), theme::removed(),
                                          codeview::kWashAmount));
    m_banner->setPalette(bannerPalette);
    m_banner->hide();
    outer->addWidget(m_banner);

    m_permission = new PermissionBar(this);
    outer->addWidget(m_permission);

    auto* bottom = new QHBoxLayout();
    bottom->setContentsMargins(4, 4, 4, 4);
    bottom->setSpacing(4);
    m_input = new PromptInput(this);
    m_interrupt = new QPushButton(QStringLiteral("Interrupt"), this);
    m_interrupt->setToolTip(QStringLiteral("End this turn; the agent stays alive"));
    m_stop = new QPushButton(QStringLiteral("Stop"), this);
    m_stop->setToolTip(QStringLiteral("Stop the agent process"));
    bottom->addWidget(m_input, 1);
    auto* buttons = new QVBoxLayout();
    buttons->setSpacing(4);
    buttons->addWidget(m_interrupt);
    buttons->addWidget(m_stop);
    bottom->addLayout(buttons, 0);
    outer->addLayout(bottom);

    // Applied on the range change rather than on the append: the scroll bar's
    // maximum is still the old one while the new frame is being laid out.
    QObject::connect(m_scroll->verticalScrollBar(), &QScrollBar::rangeChanged, this,
                     [this](int, int max) {
                         if (m_stickToBottom) {
                             m_scroll->verticalScrollBar()->setValue(max);
                         }
                     });

    if (m_model.isNull()) {
        return;
    }
    QObject::connect(model, &TranscriptModel::resetItems, this, &TranscriptView::rebuild);
    QObject::connect(model, &TranscriptModel::itemAppended, this, &TranscriptView::onItemAppended);
    QObject::connect(model, &TranscriptModel::itemChanged, this, &TranscriptView::onItemChanged);
    QObject::connect(model, &TranscriptModel::permissionRequested, this,
                     &TranscriptView::onPermissionRequested);
    QObject::connect(model, &TranscriptModel::permissionCleared, this,
                     [this] { m_permission->clear(); });
    QObject::connect(model, &TranscriptModel::errorOccurred, this, [this](const QString& message) {
        // News about a failed request, never about the transcript: the frames
        // are left exactly as they are.
        m_requestError = message;
        refreshBanner();
    });
    QObject::connect(model, &TranscriptModel::busyChanged, this, &TranscriptView::onBusyChanged);
    QObject::connect(model, &TranscriptModel::stateChanged, this, &TranscriptView::onStateChanged);
    QObject::connect(model, &TranscriptModel::stateDetailChanged, this, &TranscriptView::refreshBanner);

    QObject::connect(m_input, &PromptInput::submitted, this, [this](const QString& text) {
        if (m_model.isNull()) {
            return;
        }
        // A new prompt supersedes whatever the last request complained about.
        m_requestError.clear();
        refreshBanner();
        m_model->send(text);
    });
    QObject::connect(m_permission, &PermissionBar::allowed, this, [this](bool always) {
        if (!m_model.isNull()) {
            m_model->reply(m_permission->requestId(), true, always, QString());
        }
    });
    QObject::connect(m_permission, &PermissionBar::denied, this, [this] {
        if (!m_model.isNull()) {
            m_model->reply(m_permission->requestId(), false, false, QString());
        }
    });
    QObject::connect(m_interrupt, &QPushButton::clicked, this, [this] {
        if (!m_model.isNull()) {
            m_model->interrupt();
        }
    });
    QObject::connect(m_stop, &QPushButton::clicked, this, [this] {
        if (!m_model.isNull()) {
            m_model->stop();
        }
    });

    rebuild();
    onBusyChanged();
    onStateChanged();
}

TranscriptModel* TranscriptView::model() const { return m_model.data(); }

void TranscriptView::rebuild() {
    clearFrames();
    if (m_model.isNull()) {
        return;
    }
    const QJsonArray items =
        QJsonDocument::fromJson(m_model->itemsJson().toUtf8()).array();
    for (int i = 0; i < items.size(); ++i) {
        QWidget* frame = makeFrame(items.at(i).toObject(), i);
        m_frames.append(frame);
        m_frameLayout->insertWidget(m_frameLayout->count() - 1, frame);
    }
    // A rebuild is a fresh look at the conversation, and the foot of it is
    // where the newest message is.
    m_stickToBottom = true;
}

void TranscriptView::onItemAppended(int index) {
    if (index != m_frames.size()) {
        // The model appended somewhere this view does not have; only a full
        // rebuild can put that right.
        rebuild();
        return;
    }
    // Read before the frame goes in, while the scroll bar still describes what
    // the user is looking at.
    m_stickToBottom = atBottom();
    QWidget* frame = makeFrame(itemAt(index), index);
    m_frames.append(frame);
    m_frameLayout->insertWidget(m_frameLayout->count() - 1, frame);
}

void TranscriptView::onItemChanged(int index) {
    if (index < 0 || index >= m_frames.size()) {
        rebuild();
        return;
    }
    // A growing answer at the foot should keep following; one further up must
    // not drag the view away from what is being read.
    m_stickToBottom = atBottom();
    updateFrame(index, itemAt(index));
}

void TranscriptView::onPermissionRequested() {
    if (m_model.isNull()) {
        return;
    }
    // Read from the property, never carried in the signal: the model sets it
    // first precisely so this is the one copy.
    const QJsonObject pending =
        QJsonDocument::fromJson(m_model->getPendingJson().toUtf8()).object();
    if (pending.isEmpty()) {
        m_permission->clear();
        return;
    }
    m_permission->show(pending);
}

void TranscriptView::onBusyChanged() {
    if (m_model.isNull()) {
        return;
    }
    onStateChanged();
}

void TranscriptView::onStateChanged() {
    if (m_model.isNull()) {
        return;
    }
    const bool busy = m_model->getBusy();
    const QString state = m_model->getState();
    const bool working = state == QString::fromUtf8(kStateWorking);
    if (busy) {
        m_input->setBusy(true, QStringLiteral("replaying history…"));
    } else {
        m_input->setBusy(working);
    }
    m_interrupt->setEnabled(!busy && working);
    // Nothing left to stop once the process is gone.
    m_stop->setEnabled(!busy && state != QString::fromUtf8(kStateExited) && !state.isEmpty());
    refreshBanner();
}

void TranscriptView::refreshBanner() {
    QString text = m_requestError;
    if (text.isEmpty() && !m_model.isNull() &&
        m_model->getState() == QString::fromUtf8(kStateError)) {
        const QString detail = m_model->getStateDetail();
        text = detail.isEmpty() ? QStringLiteral("The agent reported an error.") : detail;
    }
    m_banner->setText(text);
    m_banner->setVisible(!text.isEmpty());
}

QWidget* TranscriptView::makeFrame(const QJsonObject& item, int index) {
    const QString kind = item.value(QStringLiteral("kind")).toString();
    if (kind == QStringLiteral("tool_use")) {
        auto* card = new ToolCard(item, m_scroll->widget());
        QObject::connect(card, &ToolCard::collapsedChanged, this, [this, index](bool collapsed) {
            if (!m_model.isNull()) {
                m_model->setCollapsed(index, collapsed);
            }
        });
        return card;
    }
    QFrame* frame = makeTextFrame(kind, m_scroll->widget());
    QLabel* label = bodyLabel(frame);
    if (kind == QStringLiteral("user")) {
        frame->setFrameShape(QFrame::StyledPanel);
        frame->setAutoFillBackground(true);
        QPalette framePalette = frame->palette();
        // Washed, not the raw role: what the user said should read as a block
        // without shouting over the answer to it.
        framePalette.setColor(QPalette::Window,
                              codeview::wash(palette().base().color(), theme::renamed(),
                                             codeview::kWashAmount));
        frame->setPalette(framePalette);
        label->setTextFormat(Qt::PlainText);
    } else if (kind == QStringLiteral("assistant")) {
        // The answer is Markdown, and Qt renders it: code spans, lists and
        // emphasis are the shape the model writes in.
        label->setTextFormat(Qt::MarkdownText);
    } else {
        applySmallGrey(label, kind == QStringLiteral("system"));
        label->setTextFormat(Qt::PlainText);
    }
    label->setText(bodyText(item));
    return frame;
}

void TranscriptView::updateFrame(int index, const QJsonObject& item) {
    if (index < 0 || index >= m_frames.size()) {
        return;
    }
    QWidget* frame = m_frames.at(index);
    const QString kind = item.value(QStringLiteral("kind")).toString();
    if (auto* card = qobject_cast<ToolCard*>(frame)) {
        if (kind == QStringLiteral("tool_use")) {
            card->update(item);
            return;
        }
    } else if (frame->property(kKindProperty).toString() == kind) {
        QLabel* label = bodyLabel(frame);
        if (label != nullptr) {
            label->setText(bodyText(item));
            return;
        }
    }
    // The item is no longer the kind this frame was built for. Nothing in the
    // model does that today, but a frame showing the wrong item is worse than
    // one rebuilt for nothing.
    QWidget* replacement = makeFrame(item, index);
    // `replaceWidget` hands back the old widget's layout item, which the caller
    // owns from then on.
    delete m_frameLayout->replaceWidget(frame, replacement);
    m_frames[index] = replacement;
    replacement->show();
    frame->hide();
    frame->deleteLater();
}

void TranscriptView::clearFrames() {
    for (QWidget* frame : m_frames) {
        m_frameLayout->removeWidget(frame);
        frame->hide();
        frame->deleteLater();
    }
    m_frames.clear();
}

QJsonObject TranscriptView::itemAt(int index) const {
    if (m_model.isNull()) {
        return QJsonObject();
    }
    const QString json = m_model->itemJson(index);
    if (json.isEmpty()) {
        // Empty means "no such item", not "an item that parses to nothing".
        return QJsonObject();
    }
    return QJsonDocument::fromJson(json.toUtf8()).object();
}

bool TranscriptView::atBottom() const {
    const QScrollBar* bar = m_scroll->verticalScrollBar();
    const int lineHeight = QFontMetrics(font()).lineSpacing();
    return bar->value() >= bar->maximum() - lineHeight;
}
