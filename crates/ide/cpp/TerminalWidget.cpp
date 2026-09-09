#include "TerminalWidget.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include <QColor>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QList>
#include <QPainter>
#include <QPalette>
#include <QShowEvent>
#include <QTimer>
#include <QWheelEvent>
#include <utility>

namespace {

/// Cursor blink half-period.
constexpr int kBlinkMs = 500;
/// One wheel notch is 120 units of angle delta; three lines per notch.
constexpr int kWheelUnitsPerLine = 40;
/// The error banner's colour, matching the error tab colour in `GroupBar`.
const QColor kErrorColour(0xeb, 0x57, 0x57);

/// Maps character index -> UTF-16 offset in `text`, plus a final sentinel.
///
/// The row spans count cells, and the grid emits exactly one Unicode scalar per
/// cell, so a character outside the BMP is one span position but two `QString`
/// units. Slicing through this table keeps the two in step.
QList<int> charOffsets(const QString& text) {
    QList<int> offsets;
    offsets.reserve(text.size() + 1);
    for (int i = 0; i < text.size();) {
        offsets.append(i);
        const bool surrogatePair =
            text.at(i).isHighSurrogate() && i + 1 < text.size() && text.at(i + 1).isLowSurrogate();
        i += surrogatePair ? 2 : 1;
    }
    offsets.append(text.size());
    return offsets;
}

bool isBlank(const QString& text) {
    for (const QChar c : text) {
        if (c != QLatin1Char(' ')) {
            return false;
        }
    }
    return true;
}

/// `#rrggbb` from the grid, or the palette colour the grid meant by an empty
/// string.
QColor spanColour(const QString& hex, const QColor& fallback) {
    if (hex.isEmpty()) {
        return fallback;
    }
    const QColor parsed = QColor::fromString(hex);
    return parsed.isValid() ? parsed : fallback;
}

} // namespace

TerminalWidget::TerminalWidget(TerminalSession* session, QWidget* parent)
    : QWidget(parent), m_session(session) {
    m_font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    m_font.setPointSize(10);
    m_font.setStyleHint(QFont::Monospace);
    m_font.setFixedPitch(true);
    const QFontMetrics metrics(m_font);
    m_charWidth = qMax(1, metrics.horizontalAdvance(QStringLiteral("M")));
    m_lineHeight = qMax(1, metrics.lineSpacing());
    m_ascent = metrics.ascent();

    setFocusPolicy(Qt::StrongFocus);
    // Every paint fills the whole rect, so Qt need not clear it first.
    setAttribute(Qt::WA_OpaquePaintEvent);
    setAttribute(Qt::WA_InputMethodEnabled, false);

    m_blinkTimer = new QTimer(this);
    m_blinkTimer->setInterval(kBlinkMs);
    QObject::connect(m_blinkTimer, &QTimer::timeout, this, [this] {
        m_blinkOn = !m_blinkOn;
        update();
    });

    if (m_session) {
        // `frame` means "repaint now": it fires on output, scroll, resize and
        // error alike.
        QObject::connect(m_session, &TerminalSession::frame, this, [this] { update(); });
        QObject::connect(m_session, &TerminalSession::exitedSignal, this, [this] { update(); });
        QObject::connect(m_session, &TerminalSession::errorChanged, this, [this] { update(); });
        // A resize between `open` and its answer updates the grid but cannot
        // reach a PTY that does not exist yet, and the session's own `cols`
        // and `rows` follow the grid, so they cannot tell what the daemon
        // actually created. The size is therefore restated unconditionally
        // once there is a PTY to tell.
        QObject::connect(m_session, &TerminalSession::opened, this, [this] {
            if (m_session) {
                m_session->resize(m_cols, m_rows);
            }
        });
    }
}

TerminalSession* TerminalWidget::session() const {
    return m_session.data();
}

void TerminalWidget::openSession(const QString& workspaceId, const QString& command) {
    m_pendingWorkspace = workspaceId;
    m_pendingCommand = command;
    m_pendingOpen = true;
    QTimer::singleShot(0, this, [this] { maybeOpen(); });
}

void TerminalWidget::maybeOpen() {
    if (!m_pendingOpen || !m_session) {
        return;
    }
    // A hidden widget has whatever geometry it was constructed with: a page of
    // a tab widget or a stack that has never been current is typically a few
    // cells across. Waiting for the first show costs nothing and is the only
    // way to know the size the shell should start at.
    if (!isVisible()) {
        return;
    }
    m_pendingOpen = false;
    m_cols = qMax(2, width() / m_charWidth);
    m_rows = qMax(1, height() / m_lineHeight);
    m_session->open(m_pendingWorkspace, m_cols, m_rows, m_pendingCommand);
}

QSize TerminalWidget::sizeHint() const {
    return QSize(80 * m_charWidth, 24 * m_lineHeight);
}

QSize TerminalWidget::minimumSizeHint() const {
    return QSize(8 * m_charWidth, 2 * m_lineHeight);
}

void TerminalWidget::paintEvent(QPaintEvent*) {
    QPainter painter(this);
    painter.fillRect(rect(), palette().base());
    if (!m_session) {
        return;
    }
    const QJsonArray rows = QJsonDocument::fromJson(m_session->getRowsJson().toUtf8()).array();
    painter.setFont(m_font);
    paintRows(painter, rows);
    paintCursor(painter, rows);
    if (m_session->getExited()) {
        paintExitLine(painter);
    }
    paintError(painter);
}

void TerminalWidget::paintRows(QPainter& painter, const QJsonArray& rows) {
    const QColor defaultFg = palette().text().color();
    const QColor defaultBg = palette().base().color();
    for (int r = 0; r < rows.size(); ++r) {
        const QJsonObject row = rows.at(r).toObject();
        const QString text = row.value(QStringLiteral("text")).toString();
        const QList<int> offsets = charOffsets(text);
        const int cells = offsets.size() - 1;
        const int y = r * m_lineHeight;
        if (y >= height()) {
            break;
        }
        const QJsonArray spans = row.value(QStringLiteral("spans")).toArray();
        for (const QJsonValue& value : spans) {
            const QJsonObject span = value.toObject();
            const int start = span.value(QStringLiteral("start")).toInt();
            int len = span.value(QStringLiteral("len")).toInt();
            if (start < 0 || start >= cells || len <= 0) {
                continue;
            }
            len = qMin(len, cells - start);
            const int from = offsets.at(start);
            const QString piece = text.mid(from, offsets.at(start + len) - from);

            QColor fg = spanColour(span.value(QStringLiteral("fg")).toString(), defaultFg);
            QColor bg = spanColour(span.value(QStringLiteral("bg")).toString(), defaultBg);
            if (span.value(QStringLiteral("inverse")).toBool()) {
                std::swap(fg, bg);
            }
            const bool underline = span.value(QStringLiteral("underline")).toBool();
            const QRect cell(start * m_charWidth, y, len * m_charWidth, m_lineHeight);
            painter.fillRect(cell, bg);
            // Blank runs still need their underline drawn; nothing else about
            // them is visible, so the text draw is skipped.
            if (isBlank(piece) && !underline) {
                continue;
            }
            QFont font = m_font;
            font.setBold(span.value(QStringLiteral("bold")).toBool());
            font.setItalic(span.value(QStringLiteral("italic")).toBool());
            font.setUnderline(underline);
            painter.setFont(font);
            painter.setPen(fg);
            painter.drawText(cell.x(), y + m_ascent, piece);
        }
    }
    painter.setFont(m_font);
}

void TerminalWidget::paintCursor(QPainter& painter, const QJsonArray& rows) {
    if (!m_session->getCursorVisible()) {
        return;
    }
    const int col = m_session->getCursorCol();
    const int row = m_session->getCursorRow();
    if (col < 0 || row < 0) {
        return;
    }
    const QRect cell(col * m_charWidth, row * m_lineHeight, m_charWidth, m_lineHeight);
    if (!cell.intersects(rect())) {
        return;
    }
    if (!hasFocus()) {
        // An unfocused terminal shows where the cursor is without pretending to
        // be taking input.
        painter.setPen(palette().text().color());
        painter.setBrush(Qt::NoBrush);
        painter.drawRect(cell.adjusted(0, 0, -1, -1));
        return;
    }
    if (!m_blinkOn) {
        return;
    }
    painter.fillRect(cell, palette().text().color());
    const QString text = rows.at(row).toObject().value(QStringLiteral("text")).toString();
    const QList<int> offsets = charOffsets(text);
    if (col + 1 < offsets.size()) {
        const int from = offsets.at(col);
        painter.setPen(palette().base().color());
        painter.drawText(cell.x(), cell.y() + m_ascent, text.mid(from, offsets.at(col + 1) - from));
    }
}

void TerminalWidget::paintExitLine(QPainter& painter) {
    // Pinned to the bottom edge: the rows fill the widget to within less than
    // one line, so there is never room for an extra one below them.
    const QRect line(0, qMax(0, height() - m_lineHeight), width(), m_lineHeight);
    QColor dim = palette().text().color();
    dim.setAlpha(140);
    painter.fillRect(line, palette().base());
    painter.setPen(dim);
    painter.drawText(0, line.y() + m_ascent,
                     QStringLiteral("[process exited with code %1]").arg(m_session->getExitCode()));
}

void TerminalWidget::paintError(QPainter& painter) {
    // A finished process has nothing left to fail at, and the session still
    // sends `pty.resize` for a PTY the daemon has already reaped, so every
    // resize of a dead pane would raise a banner. The exit line says what
    // happened; that is the whole story once `exited` is set.
    if (m_session->getExited()) {
        return;
    }
    const QString message = m_session->getError();
    if (message.isEmpty()) {
        return;
    }
    QFont font = m_font;
    font.setBold(true);
    painter.setFont(font);
    const QFontMetrics metrics(font);
    const QRect box = metrics.boundingRect(rect(), Qt::AlignCenter | Qt::TextWordWrap, message);
    painter.fillRect(box.adjusted(-8, -4, 8, 4), palette().base());
    painter.setPen(kErrorColour);
    painter.drawText(rect(), Qt::AlignCenter | Qt::TextWordWrap, message);
}

void TerminalWidget::keyPressEvent(QKeyEvent* event) {
    if (!m_session) {
        QWidget::keyPressEvent(event);
        return;
    }
    m_session->writeKey(event->key(), event->modifiers().toInt(), event->text());
    event->accept();
}

void TerminalWidget::wheelEvent(QWheelEvent* event) {
    if (!m_session) {
        QWidget::wheelEvent(event);
        return;
    }
    // Positive `scroll` moves towards history, and a wheel notch away from the
    // user is a positive angle delta, so the two signs already agree.
    const int lines = event->angleDelta().y() / kWheelUnitsPerLine;
    if (lines != 0) {
        m_session->scroll(lines);
    }
    event->accept();
}

void TerminalWidget::resizeEvent(QResizeEvent* event) {
    QWidget::resizeEvent(event);
    if (m_pendingOpen) {
        // The first real geometry: open at that size rather than resizing a
        // PTY that has only just been created.
        maybeOpen();
        return;
    }
    applySize();
}

void TerminalWidget::applySize() {
    const int cols = qMax(2, width() / m_charWidth);
    const int rows = qMax(1, height() / m_lineHeight);
    if (cols == m_cols && rows == m_rows) {
        return;
    }
    m_cols = cols;
    m_rows = rows;
    if (m_session) {
        m_session->resize(cols, rows);
    }
}

void TerminalWidget::focusInEvent(QFocusEvent* event) {
    QWidget::focusInEvent(event);
    m_blinkOn = true;
    m_blinkTimer->start();
    update();
}

void TerminalWidget::focusOutEvent(QFocusEvent* event) {
    QWidget::focusOutEvent(event);
    m_blinkTimer->stop();
    m_blinkOn = true;
    update();
}

void TerminalWidget::showEvent(QShowEvent* event) {
    QWidget::showEvent(event);
    // Deferred: the layout runs on a posted event, so a resize with the real
    // geometry is delivered before this fires and usually opens the session
    // first. This is the fallback for a widget that is already the right size.
    QTimer::singleShot(0, this, [this] { maybeOpen(); });
}

bool TerminalWidget::focusNextPrevChild(bool) {
    return false;
}
