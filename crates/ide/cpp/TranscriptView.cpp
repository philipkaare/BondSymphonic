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

/// HORIZONTAL ELLIPSIS, for the placeholders that say the box is waiting. A
/// code point for the same reason as the dot above: written as a literal, its
/// UTF-8 bytes are re-read in the system code page and the word ends in
/// mojibake, with no compiler warning because each byte happens to map.
constexpr char16_t kEllipsis = 0x2026;

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

    // The composer as one widget rather than a bare layout, so the login gate
    // can take its place without every child having to be hidden by hand.
    m_composer = new QWidget(this);
    auto* bottom = new QHBoxLayout(m_composer);
    bottom->setContentsMargins(4, 4, 4, 4);
    bottom->setSpacing(4);
    m_input = new PromptInput(this);
    m_interrupt = new QPushButton(QStringLiteral("Interrupt"), this);
    m_interrupt->setToolTip(QStringLiteral("End this turn; the agent stays alive"));
    m_stop = new QPushButton(QStringLiteral("Stop"), this);
    m_stop->setToolTip(QStringLiteral("Stop the agent process"));
    m_start = new QPushButton(QStringLiteral("Start agent"), this);
    // Named so a test can find it without the view growing an accessor for it.
    m_start->setObjectName(QStringLiteral("bsStartAgent"));
    m_start->setToolTip(QStringLiteral("Start a Claude agent in this workspace"));
    m_start->hide();
    bottom->addWidget(m_input, 1);
    auto* buttons = new QVBoxLayout();
    buttons->setSpacing(4);
    buttons->addWidget(m_start);
    buttons->addWidget(m_interrupt);
    buttons->addWidget(m_stop);
    bottom->addLayout(buttons, 0);
    outer->addWidget(m_composer);

    // Shown in the composer's place until `claude_auth` passes. A button and a
    // sentence, not a disabled prompt box: a greyed box invites the user to
    // wait for something that is never going to happen on its own, whereas
    // this says what is missing and leads to the one place it can be fixed.
    m_loginGate = new QWidget(this);
    auto* gateLayout = new QHBoxLayout(m_loginGate);
    gateLayout->setContentsMargins(4, 4, 4, 4);
    gateLayout->setSpacing(4);
    auto* gateText = new QLabel(
        QStringLiteral("Claude Code is not logged in, so this agent cannot answer yet."),
        m_loginGate);
    gateText->setWordWrap(true);
    gateText->setEnabled(false);
    gateLayout->addWidget(gateText, 1);
    auto* loginButton =
        new QPushButton(QStringLiteral("Log in to Claude Code") + QChar(kEllipsis), m_loginGate);
    // Named so a test can find it without the view growing an accessor for it.
    loginButton->setObjectName(QStringLiteral("bsClaudeLogin"));
    loginButton->setToolTip(QStringLiteral("Open Settings on the Setup section"));
    gateLayout->addWidget(loginButton, 0);
    m_loginGate->hide();
    outer->addWidget(m_loginGate);
    QObject::connect(loginButton, &QPushButton::clicked, this,
                     [this] { emit loginRequested(); });

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
    // Attaching is what opens the box: until the id lands there is no agent to
    // send a prompt to.
    QObject::connect(model, &TranscriptModel::agentIdChanged, this, &TranscriptView::onStateChanged);
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
    QObject::connect(m_start, &QPushButton::clicked, this, [this] {
        // The view neither knows the workspace nor talks to the controller: it
        // says what the user asked for and the area routes it.
        m_requestError.clear();
        setStarting(true);
        emit startAgentRequested();
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
    // `attach` clears the pending request without emitting `permissionCleared`,
    // so a bar left up from a previous attach would answer for a request nobody
    // is waiting on. Re-reading the property here shows or hides it either way.
    onPermissionRequested();
    // The last failed request belonged to the transcript that has just been
    // replaced.
    m_requestError.clear();
    refreshBanner();
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

void TranscriptView::setClaudeLoggedIn(bool loggedIn) {
    if (m_loggedIn == loggedIn) {
        return;
    }
    m_loggedIn = loggedIn;
    m_composer->setVisible(loggedIn);
    m_loginGate->setVisible(!loggedIn);
    // The permission bar stays where it is: answering Allow or Deny is not
    // sending a prompt, and an agent that is already running can still be
    // waiting on one when a token expires underneath it.
    if (m_model.isNull()) {
        return;
    }
    // The composer coming back has to arrive with the right busy state on it,
    // which is whatever the model says now rather than what it said when the
    // gate went up.
    onStateChanged();
}

void TranscriptView::setStarting(bool starting) {
    const QString agentId = m_model.isNull() ? QString() : m_model->getAgentId();
    if (m_starting == starting && (!starting || m_startFromAgentId == agentId)) {
        return;
    }
    m_starting = starting;
    if (starting) {
        m_startFromAgentId = agentId;
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
    const bool exited = state == QString::fromUtf8(kStateExited);
    // A model that has never been attached answers `idle` and `not busy`, which
    // would leave the box live in the seconds `agent.start` takes. `send` then
    // drops the text, because there is no agent to send it to, and the prompt
    // the user typed while waiting disappears without a word. Waiting for the
    // id is the one condition that covers the box and both buttons.
    const QString agentId = m_model->getAgentId();
    // The start answered: a different agent is attached than the one the pane
    // had when it was asked for. Self-correcting, so a lost `operationFailed`
    // cannot strand the pane on "starting" forever.
    if (m_starting && agentId != m_startFromAgentId) {
        m_starting = false;
    }
    const bool noAgent = agentId.isEmpty();
    // A pane with no agent and no start in flight is not starting -- it is
    // empty, which is what a restored session or a stopped agent leaves, and
    // saying "starting the agent" there is a promise nothing will keep.
    const bool startable = (noAgent || exited) && !m_starting;
    m_start->setVisible(startable);
    m_start->setText(exited ? QStringLiteral("Restart agent") : QStringLiteral("Start agent"));
    if (m_starting) {
        m_input->setBusy(true, QStringLiteral("starting the agent") + QChar(kEllipsis));
    } else if (noAgent) {
        m_input->setBusy(true, QStringLiteral("no agent is running in this workspace"));
    } else if (busy) {
        m_input->setBusy(true, QStringLiteral("replaying history") + QChar(kEllipsis));
    } else {
        m_input->setBusy(working);
    }
    m_interrupt->setEnabled(!noAgent && !m_starting && !busy && working);
    // Nothing left to stop once the process is gone, and nothing yet to stop
    // before it exists.
    m_stop->setEnabled(!noAgent && !m_starting && !busy && !exited && !state.isEmpty());
    refreshBanner();
}

void TranscriptView::refreshBanner() {
    QString text = m_requestError;
    if (text.isEmpty() && !m_model.isNull()) {
        const QString state = m_model->getState();
        const QString detail = m_model->getStateDetail();
        if (state == QString::fromUtf8(kStateError)) {
            text = detail.isEmpty() ? QStringLiteral("The agent reported an error.") : detail;
        } else if (state == QString::fromUtf8(kStateExited) && !detail.isEmpty()) {
            // An agent that dies at start-up -- not logged in, no API key, a
            // bad flag -- exits rather than erroring, and the daemon puts the
            // reason in this detail. Without the banner the tab simply goes
            // quiet and the user is told nothing at all.
            text = QStringLiteral("agent exited: ") + detail;
        }
    }
    m_banner->setText(text);
    m_banner->setVisible(!text.isEmpty());
}

QWidget* TranscriptView::makeFrame(const QJsonObject& item, int index) {
    const QString kind = item.value(QStringLiteral("kind")).toString();
    if (kind == QStringLiteral("earlier")) {
        // The fold. One widget standing for however many items the model took
        // off the top of a long conversation; clicking it puts them all back.
        auto* frame = new QFrame(m_scroll->widget());
        frame->setProperty(kKindProperty, kind);
        frame->setFrameShape(QFrame::NoFrame);
        auto* layout = new QHBoxLayout(frame);
        layout->setContentsMargins(6, 2, 6, 2);
        auto* button = new QPushButton(item.value(QStringLiteral("text")).toString(), frame);
        button->setObjectName(QStringLiteral("TranscriptLoadEarlier"));
        button->setFlat(true);
        layout->addWidget(button, 0);
        layout->addStretch(1);
        QObject::connect(button, &QPushButton::clicked, this, [this] {
            if (!m_model.isNull()) {
                // Answers with `resetItems`, which rebuilds this whole column.
                m_model->expandEarlier();
            }
        });
        return frame;
    }
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
