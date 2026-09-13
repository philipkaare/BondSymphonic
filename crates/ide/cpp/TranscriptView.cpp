#include "TranscriptView.h"
#include "AgentChoices.h"
#include "CodeView.h"
#include "PermissionBar.h"
#include "PromptInput.h"
#include "Theme.h"
#include "ToolCard.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QChar>
#include <QComboBox>
#include <QEvent>
#include <QFont>
#include <QFontMetrics>
#include <QFrame>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLabel>
#include <QLineEdit>
#include <QPalette>
#include <QPushButton>
#include <QScrollArea>
#include <QScrollBar>
#include <QTimer>
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

/// How long changed frames are collected before they are repainted.
///
/// Long enough that a fast stream costs twenty repaints a second rather than
/// one per delta, short enough that the answer still appears to arrive as it is
/// written. It bounds the lag of every in-place update, not just a streamed
/// one, which is why it is a fraction of a reading pause and not a second.
constexpr int kCoalesceMs = 50;

/// The daemon's state words this view reacts to. Anything else is a state it
/// has nothing to say about, which is the right answer for a word added later.
/// The two item kinds `setShowMeta` hides: what the turn cost, and what the
/// agent's own start-up reported. They are the only two that go through
/// `applySmallGrey`, which is the same observation from the other side.
const char* kKindResult = "result";
const char* kKindSystem = "system";

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

/// How an assistant answer's label is painted: as Markdown once the answer is
/// finished, as plain text while it is still arriving.
///
/// Qt reparses the whole Markdown document and relays the label out on every
/// `setText`, so rendering a half-written answer costs that for each delta and
/// throws the result away a few milliseconds later. Plain text says the same
/// words; the formatting lands with the last one.
Qt::TextFormat assistantFormat(const QJsonObject& item) {
    return item.value(QStringLiteral("streaming")).toBool() ? Qt::PlainText : Qt::MarkdownText;
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
    // At the head of the column, before the stretch and before any frame, so
    // the first real message appears under it and then takes its place. It is
    // laid out here rather than above the scroll because an empty pane and a
    // pane with one message should not put their first line in two different
    // places.
    m_welcome = new QWidget(column);
    auto* welcomeLayout = new QVBoxLayout(m_welcome);
    welcomeLayout->setContentsMargins(6, 8, 6, 8);
    welcomeLayout->setSpacing(2);
    m_welcomeReady = new QLabel(m_welcome);
    m_welcomeWhat = new QLabel(m_welcome);
    m_welcomeWhere = new QLabel(m_welcome);
    for (QLabel* line : { m_welcomeReady, m_welcomeWhat, m_welcomeWhere }) {
        line->setTextFormat(Qt::PlainText);
        line->setWordWrap(true);
        welcomeLayout->addWidget(line);
    }
    // The same grey the result and system lines wear. What the pane is about to
    // do is not news, and it must not read as the agent's first answer.
    applySmallGrey(m_welcomeWhat, false);
    applySmallGrey(m_welcomeWhere, false);
    m_welcome->hide();
    m_frameLayout->addWidget(m_welcome);
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
    applyBannerWash();
    m_banner->hide();
    outer->addWidget(m_banner);

    m_permission = new PermissionBar(this);
    outer->addWidget(m_permission);

    // The composer as one widget rather than a bare layout, so the login gate
    // can take its place without every child having to be hidden by hand.
    m_composer = new QWidget(this);
    auto* bottom = new QVBoxLayout(m_composer);
    bottom->setContentsMargins(4, 4, 4, 4);
    bottom->setSpacing(4);
    m_input = new PromptInput(this);
    bottom->addWidget(m_input, 1);

    // The second row: what this agent is, and the one button that acts on the
    // conversation rather than on the process. Start and Stop stood here once;
    // a workspace starts its own agent now, and killing a process is not
    // something anybody wants from the place they type a sentence.
    auto* choices = new QHBoxLayout();
    choices->setSpacing(4);
    m_modelChoice = new QComboBox(m_composer);
    m_modelChoice->setObjectName(QStringLiteral("TranscriptModelChoice"));
    m_modelChoice->setToolTip(QStringLiteral(
        "The model this agent answers with. Changing it restarts the agent and resumes the "
        "conversation; a turn in flight is interrupted."));
    agentchoices::fillModelCombo(m_modelChoice, QString());
    // Nothing typed here joins the list: an id that only ever existed in one
    // pane would outlive that pane and be offered to the next agent as though
    // somebody had chosen it.
    m_modelChoice->setInsertPolicy(QComboBox::NoInsert);
    m_permissionChoice = new QComboBox(m_composer);
    m_permissionChoice->setObjectName(QStringLiteral("TranscriptPermissionChoice"));
    m_permissionChoice->setToolTip(QStringLiteral(
        "What this agent asks before it acts. Changing it restarts the agent and resumes the "
        "conversation."));
    agentchoices::fillPermissionCombo(m_permissionChoice,
                                      QString::fromUtf8(agentchoices::defaultPermissionMode()));
    m_interrupt = new QPushButton(QStringLiteral("Interrupt"), m_composer);
    m_interrupt->setToolTip(QStringLiteral("End this turn; the agent stays alive"));
    // Both combos share what is left after Interrupt, and both may shrink well
    // below the width their longest entry wants -- "Default (Claude Code
    // decides)" is a wide thing to demand of a dock somebody has dragged narrow.
    // Without this the row sets a floor under the whole pane's width.
    for (QComboBox* choice : { m_modelChoice, m_permissionChoice }) {
        choice->setSizeAdjustPolicy(QComboBox::AdjustToMinimumContentsLengthWithIcon);
        choice->setMinimumContentsLength(6);
    }
    choices->addWidget(m_modelChoice, 1);
    choices->addWidget(m_permissionChoice, 1);
    choices->addWidget(m_interrupt, 0);
    bottom->addLayout(choices, 0);
    outer->addWidget(m_composer);

    QObject::connect(m_modelChoice, &QComboBox::currentIndexChanged, this, [this](int) {
        emitOptionsChanged();
        updateWelcome();
    });
    QObject::connect(m_permissionChoice, &QComboBox::currentIndexChanged, this, [this](int) {
        emitOptionsChanged();
        updateWelcome();
    });
    if (QLineEdit* typed = m_modelChoice->lineEdit()) {
        // A model name typed rather than picked is still a choice, and the CLI
        // takes names this build has never heard of. On the edit being finished
        // rather than on every keystroke: half a model name is not a model.
        QObject::connect(typed, &QLineEdit::editingFinished, this,
                         [this] { emitOptionsChanged(); });
    }

    // Shown in the composer's place until `claude_auth` passes. A button and a
    // sentence, not a disabled prompt box: a greyed box invites the user to
    // wait for something that is never going to happen on its own, whereas
    // this says what is missing and leads to the one place it can be fixed.
    m_loginGate = new QWidget(this);
    // Stacked, not side by side. The agent pane is a dock now and is routinely
    // a couple of hundred pixels wide; beside a button whose text sets its
    // width, a wrapped sentence is squeezed into whatever is left and comes out
    // one word per line. Nothing about that is visible until the pane is
    // narrow, which is why it survived the offscreen checks and did not survive
    // looking at it.
    auto* gateLayout = new QVBoxLayout(m_loginGate);
    gateLayout->setContentsMargins(4, 4, 4, 4);
    gateLayout->setSpacing(4);
    auto* gateText = new QLabel(
        QStringLiteral("Claude Code is not logged in, so this agent cannot answer yet."),
        m_loginGate);
    gateText->setWordWrap(true);
    gateText->setEnabled(false);
    gateLayout->addWidget(gateText);
    auto* loginButton =
        new QPushButton(QStringLiteral("Log in to Claude Code") + QChar(kEllipsis), m_loginGate);
    // Named so a test can find it without the view growing an accessor for it.
    loginButton->setObjectName(QStringLiteral("bsClaudeLogin"));
    loginButton->setToolTip(QStringLiteral("Open Settings on the Setup section"));
    gateLayout->addWidget(loginButton, 0, Qt::AlignLeft);
    m_loginGate->hide();
    outer->addWidget(m_loginGate);
    QObject::connect(loginButton, &QPushButton::clicked, this,
                     [this] { emit loginRequested(); });

    m_coalesce = new QTimer(this);
    m_coalesce->setSingleShot(true);
    m_coalesce->setInterval(kCoalesceMs);
    QObject::connect(m_coalesce, &QTimer::timeout, this, &TranscriptView::flushChangedItems);

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
    // The tab's options are set on the model after this view is built, and
    // again every time a restart lands, so the dropdowns follow the property
    // rather than being filled once from whatever it said at construction.
    QObject::connect(model, &TranscriptModel::optionsJsonChanged, this,
                     &TranscriptView::applyOptionsToChoices);

    applyOptionsToChoices();
    rebuild();
    onBusyChanged();
    onStateChanged();
}

TranscriptModel* TranscriptView::model() const { return m_model.data(); }

void TranscriptView::applyBannerWash() {
    // A palette of one role, rather than the banner's own with that role
    // overwritten: everything else the label paints with is the pane's, and
    // must stay the pane's when the pane changes.
    QPalette bannerPalette;
    bannerPalette.setColor(QPalette::Window, codeview::wash(palette().base().color(),
                                                           theme::removed(),
                                                           codeview::kWashAmount));
    m_banner->setPalette(bannerPalette);
}

void TranscriptView::changeEvent(QEvent* event) {
    QWidget::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyBannerWash();
    }
}

void TranscriptView::rebuild() {
    clearFrames();
    if (m_model.isNull()) {
        return;
    }
    const QJsonArray items = readItems();
    for (int i = 0; i < items.size(); ++i) {
        QWidget* frame = makeFrame(items.at(i).toObject(), i);
        // Appended either way: the list is indexed by item, and a hidden meta
        // line keeps its slot so every index after it still names its own
        // frame.
        m_frames.append(frame);
        if (frame != nullptr) {
            m_frameLayout->insertWidget(m_frameLayout->count() - 1, frame);
        }
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
    updateWelcome();
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
    if (frame != nullptr) {
        m_frameLayout->insertWidget(m_frameLayout->count() - 1, frame);
    }
    // The first real message is what the welcome was standing in for.
    updateWelcome();
}

void TranscriptView::onItemChanged(int index) {
    if (index < 0 || index >= m_frames.size()) {
        rebuild();
        return;
    }
    // A growing answer at the foot should keep following; one further up must
    // not drag the view away from what is being read. Read now rather than at
    // the flush: this is where the scroll bar still describes what the user was
    // looking at when the change arrived.
    m_stickToBottom = atBottom();
    // Marked, not applied. Nothing is read out of the model here: a streamed
    // answer lands as one of these per delta, and the item is only worth
    // reading once per repaint.
    m_changedItems.insert(index);
    if (!m_coalesce->isActive()) {
        m_coalesce->start();
    }
}

void TranscriptView::flushChangedItems() {
    if (m_changedItems.isEmpty()) {
        return;
    }
    const QList<int> indices = m_changedItems.values();
    m_changedItems.clear();
    for (const int index : indices) {
        if (index >= 0 && index < m_frames.size()) {
            updateFrame(index, itemAt(index));
        }
    }
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
    // A model that has never been attached answers `unavailable` and `not
    // busy`, which is neither working nor exited: without the id below the box
    // would stay live through the seconds `agent.start` takes. `send` then
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
    // Shut only while a start is already in flight or the history is still
    // being replayed. A pane whose agent has exited keeps them open on purpose:
    // choosing the model it should come back on is a reasonable way to ask for
    // it back.
    const bool choosable = !m_starting && !busy;
    m_modelChoice->setEnabled(choosable);
    m_permissionChoice->setEnabled(choosable);
    refreshBanner();
}

void TranscriptView::setShowMeta(bool show) {
    if (m_showMeta == show) {
        return;
    }
    m_showMeta = show;
    // A rebuild rather than a sweep over the frames: the setting changes which
    // items have a frame at all, and rebuilding is the one path that already
    // knows how to answer that question for every item at once.
    rebuild();
}

bool TranscriptView::isHiddenMeta(const QString& kind) const {
    if (m_showMeta) {
        return false;
    }
    return kind == QString::fromUtf8(kKindResult) || kind == QString::fromUtf8(kKindSystem);
}

int TranscriptView::shownFrames() const {
    int shown = 0;
    for (QWidget* frame : m_frames) {
        if (frame != nullptr) {
            ++shown;
        }
    }
    return shown;
}

int TranscriptView::columnPositionFor(int index) const {
    // The welcome is the column's first entry and the stretch its last, so a
    // frame's place is one past the welcome plus however many frames before it
    // exist.
    int position = 1;
    for (int i = 0; i < index && i < m_frames.size(); ++i) {
        if (m_frames.at(i) != nullptr) {
            ++position;
        }
    }
    return position;
}

void TranscriptView::setWelcome(const QString& name, const QString& origin,
                                const QString& worktree) {
    m_welcomeName = name;
    m_welcomeOrigin = origin;
    m_welcomeWorktree = worktree;
    updateWelcome();
}

void TranscriptView::updateWelcome() {
    // Nothing to say without a name, and nothing to say over a conversation
    // that has already started.
    if (m_welcomeName.isEmpty() || shownFrames() > 0) {
        m_welcome->hide();
        return;
    }
    m_welcomeReady->setText(m_welcomeName + QStringLiteral(" is ready."));
    const QString dot = QStringLiteral(" ") + QChar(kMiddleDot) + QStringLiteral(" ");
    // Read off the dropdowns rather than off the options, so the line says what
    // the pane is showing even in the seconds between a switch being chosen and
    // the restarted agent coming back with it.
    const QString model = chosenModelId();
    QStringList what{
        agentchoices::labelForModel(model == QStringLiteral("-") ? QString() : model),
        agentchoices::labelForPermissionMode(m_permissionChoice->currentData().toString())
    };
    if (!m_welcomeOrigin.isEmpty()) {
        what.append(m_welcomeOrigin);
    }
    m_welcomeWhat->setText(what.join(dot));
    m_welcomeWhere->setText(m_welcomeWorktree);
    m_welcomeWhere->setVisible(!m_welcomeWorktree.isEmpty());
    m_welcome->show();
}

void TranscriptView::applyOptionsToChoices() {
    if (m_model.isNull()) {
        return;
    }
    const QJsonObject options =
        QJsonDocument::fromJson(m_model->getOptionsJson().toUtf8()).object();
    const QString model = options.value(QStringLiteral("model")).toString();
    // Options with no mode in them are older than the rule that the mode is
    // always sent. Showing the floor is honest about what such an agent got:
    // the CLI's own default is that same mode, spelled its other way.
    const QString mode = options.value(QStringLiteral("permission_mode")).toString();
    m_applyingOptions = true;
    agentchoices::fillModelCombo(m_modelChoice, model);
    agentchoices::fillPermissionCombo(
        m_permissionChoice,
        mode.isEmpty() ? QString::fromUtf8(agentchoices::defaultPermissionMode()) : mode);
    m_applyingOptions = false;
    m_sentModel = chosenModelId();
    m_sentMode = m_permissionChoice->currentData().toString();
    updateWelcome();
}

QString TranscriptView::chosenModelId() const {
    const QString shown = m_modelChoice->currentText().trimmed();
    // A label off the list stands for the id behind it -- nobody types "Opus 5"
    // at a CLI -- and anything else is an id typed by hand.
    const int listed = m_modelChoice->findText(shown);
    const QString id = listed >= 0 ? m_modelChoice->itemData(listed).toString() : shown;
    // See the bridge: an empty string there means "leave the stored model
    // alone", so the one id that really is empty needs a word of its own.
    return id.isEmpty() ? QStringLiteral("-") : id;
}

void TranscriptView::emitOptionsChanged() {
    if (m_applyingOptions || m_model.isNull()) {
        return;
    }
    const QString model = chosenModelId();
    const QString mode = m_permissionChoice->currentData().toString();
    if (model == m_sentModel && mode == m_sentMode) {
        // Nothing moved. An editable combo reports its text again every time
        // the focus leaves it, and a restart is the most expensive possible
        // answer to a user who tabbed past a field.
        return;
    }
    m_sentModel = model;
    m_sentMode = mode;
    emit optionsChanged(m_model->restartOptionsChoosing(model, mode));
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
    if (isHiddenMeta(kind)) {
        // No frame at all rather than a hidden one: the setting is about a pane
        // with less in it, and a hidden widget still costs a layout pass on
        // every resize of a conversation that may be a thousand items long.
        return nullptr;
    }
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
        // emphasis are the shape the model writes in. Not while it is still
        // arriving, though; see `assistantFormat`.
        label->setTextFormat(assistantFormat(item));
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
    if (frame == nullptr) {
        // A meta line the setting is hiding. It stays hidden while it is still
        // one; an item that has changed into something else gets its frame
        // built now, in its own place rather than at the foot of the column.
        QWidget* built = makeFrame(item, index);
        if (built == nullptr) {
            return;
        }
        m_frameLayout->insertWidget(columnPositionFor(index), built);
        m_frames[index] = built;
        updateWelcome();
        return;
    }
    if (auto* card = qobject_cast<ToolCard*>(frame)) {
        if (kind == QStringLiteral("tool_use")) {
            card->update(item);
            return;
        }
    } else if (frame->property(kKindProperty).toString() == kind) {
        QLabel* label = bodyLabel(frame);
        if (label != nullptr) {
            if (kind == QStringLiteral("assistant")) {
                // The one crossing that matters: the last delta of an answer is
                // followed by the finished text with `streaming` false, and
                // that is where the Markdown is rendered -- once.
                const Qt::TextFormat format = assistantFormat(item);
                if (label->textFormat() != format) {
                    label->setTextFormat(format);
                }
            }
            label->setText(bodyText(item));
            return;
        }
    }
    // The item is no longer the kind this frame was built for. Nothing in the
    // model does that today, but a frame showing the wrong item is worse than
    // one rebuilt for nothing.
    QWidget* replacement = makeFrame(item, index);
    if (replacement == nullptr) {
        // It has become one of the two kinds the meta setting is hiding. The
        // slot stays, holding nothing, so every index after it still names its
        // own frame.
        m_frameLayout->removeWidget(frame);
        m_frames[index] = nullptr;
        frame->hide();
        frame->deleteLater();
        updateWelcome();
        return;
    }
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
        if (frame == nullptr) {
            continue;
        }
        m_frameLayout->removeWidget(frame);
        frame->hide();
        frame->deleteLater();
    }
    m_frames.clear();
    // The indices waiting for a repaint named frames that no longer exist.
    m_changedItems.clear();
    m_coalesce->stop();
}

int TranscriptView::jsonReadCount() const { return m_jsonReads; }

QJsonArray TranscriptView::readItems() const {
    ++m_jsonReads;
#if defined(BS_WIDGET_TESTS)
    if (m_testItemsSet) {
        return QJsonDocument::fromJson(
                   (QStringLiteral("[") + m_testItems.join(QLatin1Char(',')) +
                    QStringLiteral("]"))
                       .toUtf8())
            .array();
    }
#endif
    if (m_model.isNull()) {
        return QJsonArray();
    }
    return QJsonDocument::fromJson(m_model->itemsJson().toUtf8()).array();
}

QJsonObject TranscriptView::itemAt(int index) const {
    ++m_jsonReads;
#if defined(BS_WIDGET_TESTS)
    if (m_testItemsSet) {
        return index >= 0 && index < m_testItems.size()
                   ? QJsonDocument::fromJson(m_testItems.at(index).toUtf8()).object()
                   : QJsonObject();
    }
#endif
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

#if defined(BS_WIDGET_TESTS)

void TranscriptView::setTestItems(const QStringList& items) {
    m_testItems = items;
    m_testItemsSet = true;
}

int TranscriptView::frameCount() const { return shownFrames(); }

QWidget* TranscriptView::frameAt(int index) const {
    return index >= 0 && index < m_frames.size() ? m_frames.at(index) : nullptr;
}

QString TranscriptView::welcomeTextForTest() const {
    // `isHidden` rather than `isVisible`: an offscreen check never shows a
    // window, so every widget in it is invisible and only the explicit hide
    // says anything.
    if (m_welcome->isHidden()) {
        return QString();
    }
    return QStringList{ m_welcomeReady->text(), m_welcomeWhat->text(),
                        m_welcomeWhere->text() }
        .join(QLatin1Char('\n'));
}

#endif // BS_WIDGET_TESTS

bool TranscriptView::atBottom() const {
    const QScrollBar* bar = m_scroll->verticalScrollBar();
    const int lineHeight = QFontMetrics(font()).lineSpacing();
    return bar->value() >= bar->maximum() - lineHeight;
}

// --- offscreen test entries --------------------------------------------------
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)
#include <QCoreApplication>
#include <QJsonValue>
#include <QLatin1Char>
#include <QThread>
#include <cstdint>

namespace {

/// One assistant item as the model serialises it, `streaming` telling the view
/// whether more of it is still coming.
QString assistantItem(const QString& text, bool streaming) {
    QJsonObject item;
    item.insert(QStringLiteral("kind"), QStringLiteral("assistant"));
    item.insert(QStringLiteral("text"), text);
    item.insert(QStringLiteral("streaming"), streaming);
    return QString::fromUtf8(QJsonDocument(item).toJson(QJsonDocument::Compact));
}

/// How many deltas the check streams. A real answer of a few hundred words
/// arrives in about this many.
constexpr int kStreamedDeltas = 2000;

/// The most reads the whole episode may cost. One for the rebuild, a handful
/// for the repaints the timer allows, one for the finished answer -- and
/// nowhere near one per delta, which is the thing that was wrong.
constexpr int kReadBudget = 16;

/// Lets the coalescing timer fire and the flush it schedules run.
void settle() {
    QThread::msleep(kCoalesceMs * 3);
    QCoreApplication::processEvents();
}

} // namespace

/// Two thousand deltas of one answer. The view used to read the item back out
/// of the model, parse it and re-render its Markdown for every one of them,
/// which is quadratic in the length of the answer and is what made a long reply
/// slow down the whole window as it was written.
extern "C" std::int32_t bs_widget_test_transcript_coalesces_a_streamed_answer() {
    // A model with no daemon behind it: the view is driven through the same
    // signals the model emits, and the items it reads come from the seam,
    // because filling a real transcript needs an agent.
    TranscriptModel model;
    TranscriptView view(&model);
    view.resize(480, 320);

    QString text = QStringLiteral("A");
    QStringList items;
    items.append(assistantItem(text, true));
    view.setTestItems(items);
    model.resetItems();
    if (view.frameCount() != 1) {
        return 1;
    }

    const int before = view.jsonReadCount();
    for (int i = 0; i < kStreamedDeltas; ++i) {
        text += QLatin1Char('x');
        items[0] = assistantItem(text, true);
        view.setTestItems(items);
        model.itemChanged(0);
    }
    settle();

    const int reads = view.jsonReadCount() - before;
    if (reads > kReadBudget) {
        // The count is the report: 2000 says the view read the item for every
        // delta, which is the defect this check is about.
        return 100 + (reads > 1000 ? 1000 : reads);
    }

    QLabel* label = bodyLabel(view.frameAt(0));
    if (label == nullptr) {
        return 2;
    }
    // Every delta is in the text the user is left looking at: coalescing may
    // skip repaints, never content.
    if (label->text() != text) {
        return 3;
    }
    // Still arriving, so still plain: the Markdown parse is what costs, and
    // nothing about a half-written answer is worth paying it for.
    if (label->textFormat() != Qt::PlainText) {
        return 4;
    }

    // The finished answer. This is the one that renders.
    items[0] = assistantItem(text, false);
    view.setTestItems(items);
    model.itemChanged(0);
    settle();
    label = bodyLabel(view.frameAt(0));
    if (label == nullptr || label->text() != text) {
        return 5;
    }
    if (label->textFormat() != Qt::MarkdownText) {
        return 6;
    }
    return 0;
}

/// The composer chooses what this agent is, and says so as options rather than
/// as a choice.
///
/// `claude -p` reads `--model` and `--permission-mode` once, when the process
/// starts, so neither can be changed in flight: what a dropdown here means is a
/// restart that resumes the conversation, and the options that describe it can
/// only be built where the session id is. Start and Stop are gone from this row
/// -- a workspace starts its own agent, and killing a process is not something
/// anybody wants from the place they type a sentence -- and Interrupt stays,
/// because ending a turn acts on the conversation rather than on the agent.
extern "C" std::int32_t bs_widget_test_transcript_composer_offers_model_and_mode() {
    TranscriptModel model;
    model.setOptionsJson(
        QStringLiteral(R"({"model":"claude-opus-5","permission_mode":"manual"})"));
    TranscriptView view(&model);

    auto* models = view.findChild<QComboBox*>(QStringLiteral("TranscriptModelChoice"));
    auto* modes = view.findChild<QComboBox*>(QStringLiteral("TranscriptPermissionChoice"));
    if (models == nullptr || modes == nullptr) {
        return 1;
    }
    if (view.findChild<QPushButton*>(QStringLiteral("bsStartAgent")) != nullptr) {
        return 2;
    }

    // Seeded from the tab's own options, and in silence: a pane shown what it
    // already runs on has not been asked for anything.
    QStringList sent;
    QObject::connect(&view, &TranscriptView::optionsChanged, &view,
                     [&sent](const QString& optionsJson) { sent.append(optionsJson); });
    if (models->currentData().toString() != QLatin1String("claude-opus-5") ||
        modes->currentData().toString() != QLatin1String("manual")) {
        return 3;
    }
    if (!sent.isEmpty()) {
        return 4;
    }

    modes->setCurrentIndex(modes->findData(QStringLiteral("bypassPermissions")));
    if (sent.size() != 1) {
        return 5;
    }
    const QJsonObject chosen =
        QJsonDocument::fromJson(sent.at(0).toUtf8()).object();
    if (chosen.value(QStringLiteral("permission_mode")).toString() !=
        QLatin1String("bypassPermissions")) {
        return 6;
    }
    if (chosen.value(QStringLiteral("model")).toString() != QLatin1String("claude-opus-5")) {
        // One field changes; the rest of the tab's options go along untouched,
        // which is what makes this a switch rather than a new agent.
        return 7;
    }

    models->setCurrentIndex(models->findData(QStringLiteral("claude-haiku-4-5-20251001")));
    if (sent.size() != 2) {
        return 8;
    }
    const QJsonObject switched =
        QJsonDocument::fromJson(sent.at(1).toUtf8()).object();
    if (switched.value(QStringLiteral("model")).toString() !=
        QLatin1String("claude-haiku-4-5-20251001") ||
        switched.value(QStringLiteral("permission_mode")).toString() !=
            QLatin1String("bypassPermissions")) {
        return 9;
    }

    // "Let Claude Code decide" is a choice, not an absence: the key goes away
    // rather than being sent empty, and it has to reach the merge as something
    // the merge can tell apart from "leave this one alone".
    models->setCurrentIndex(models->findData(QString()));
    if (sent.size() != 3) {
        return 10;
    }
    const QJsonObject undecided =
        QJsonDocument::fromJson(sent.at(2).toUtf8()).object();
    if (undecided.contains(QStringLiteral("model"))) {
        return 11;
    }

    // The tab's options arriving again -- which is what a restart landing looks
    // like -- resets the row without asking for another one.
    model.setOptionsJson(
        QStringLiteral(R"({"model":"claude-sonnet-5","permission_mode":"plan"})"));
    if (models->currentData().toString() != QLatin1String("claude-sonnet-5") ||
        modes->currentData().toString() != QLatin1String("plan")) {
        return 12;
    }
    if (sent.size() != 3) {
        return 13;
    }
    return 0;
}

/// A pane with nothing in it says what is waiting, and stops saying it the
/// moment there is something to read instead.
///
/// An empty transcript said nothing at all, which after the Start button went
/// away left a blank pane over a prompt box. What it says is written here and
/// not by the daemon -- the daemon has nothing to say until the agent speaks --
/// and it is a frame rather than an item, so it is never persisted, never
/// replayed, and never something the user scrolls back past a week later.
extern "C" std::int32_t bs_widget_test_transcript_welcomes_an_empty_pane() {
    TranscriptModel model;
    model.setOptionsJson(
        QStringLiteral(R"({"model":"claude-opus-5","permission_mode":"bypassPermissions"})"));
    TranscriptView view(&model);
    view.setWelcome(QStringLiteral("agent-4"), QStringLiteral("BondSymphonic @ main"),
                    QStringLiteral("/home/bs/.bondsymphonic/worktrees/ws_1"));
    view.setTestItems(QStringList());
    model.resetItems();

    const QString shown = view.welcomeTextForTest();
    if (!shown.contains(QLatin1String("agent-4 is ready."))) {
        return 1;
    }
    if (!shown.contains(QLatin1String("BondSymphonic @ main")) ||
        !shown.contains(QLatin1String("/home/bs/.bondsymphonic/worktrees/ws_1"))) {
        return 2;
    }
    // What it will answer as, in the words the dropdowns use rather than in the
    // ids the CLI takes: a line the user reads is not a command line.
    if (!shown.contains(QLatin1String("Opus 5")) ||
        !shown.contains(QLatin1String("YOLO (sandboxed)"))) {
        return 3;
    }

    // The first real message takes its place: a welcome that stays is a frame
    // the user scrolls past for the rest of the conversation.
    view.setTestItems(QStringList{ QStringLiteral(R"({"kind":"user","text":"hello"})") });
    model.resetItems();
    if (!view.welcomeTextForTest().isEmpty()) {
        return 4;
    }
    if (view.frameCount() != 1) {
        return 5;
    }

    // And comes back for a transcript that empties, which is what attaching to
    // a fresh agent does to a pane that has been used.
    view.setTestItems(QStringList());
    model.resetItems();
    if (view.welcomeTextForTest().isEmpty()) {
        return 6;
    }

    // A view nobody has described says nothing rather than "  is ready.".
    TranscriptModel bare;
    TranscriptView unnamed(&bare);
    if (!unnamed.welcomeTextForTest().isEmpty()) {
        return 7;
    }
    return 0;
}

/// The small grey lines can be taken away without taking the conversation with
/// them.
///
/// What a turn cost and what the agent's own start-up reported are useful while
/// you are learning what an agent costs and noise for ever after. Hidden means
/// no frame at all rather than a hidden one -- a widget that is never painted
/// still costs a layout pass on every resize, and a long conversation has
/// hundreds of these -- so the list the view keeps is indexed by item and holds
/// nothing in the slots the setting is suppressing.
extern "C" std::int32_t bs_widget_test_transcript_hides_the_small_grey_lines() {
    TranscriptModel model;
    TranscriptView view(&model);
    view.setTestItems(QStringList{
        QStringLiteral(R"({"kind":"assistant","text":"done","streaming":false})"),
        QStringLiteral(R"({"kind":"result","cost_usd":0.0042,"duration_ms":1200,"num_turns":3})"),
        QStringLiteral(R"({"kind":"system","text":"session started"})"),
        QStringLiteral(R"({"kind":"user","text":"and again"})"),
    });
    model.resetItems();
    if (view.frameCount() != 4) {
        return 1;
    }

    view.setShowMeta(false);
    if (view.frameCount() != 2) {
        // The answer and the prompt stay; the turn cost and the system line are
        // what the setting is about.
        return 2;
    }
    // The slots the hidden items keep are what holds the rest in place: the
    // prompt after them must still be the prompt and not the answer.
    QLabel* last = bodyLabel(view.frameAt(3));
    if (last == nullptr || last->text() != QLatin1String("and again")) {
        return 3;
    }
    if (view.frameAt(1) != nullptr || view.frameAt(2) != nullptr) {
        return 4;
    }

    view.setShowMeta(true);
    if (view.frameCount() != 4) {
        return 5;
    }
    QLabel* cost = bodyLabel(view.frameAt(1));
    if (cost == nullptr || !cost->text().contains(QLatin1String("turn 3"))) {
        // Back in its own place, not appended at the foot.
        return 6;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
