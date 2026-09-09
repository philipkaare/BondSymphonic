#include "DiffWidget.h"
#include "CodeView.h"
#include "RustHighlighter.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/diff_document.cxxqt.h"
#include <QChar>
#include <QColor>
#include <QFont>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLabel>
#include <QLatin1Char>
#include <QPalette>
#include <QScrollBar>
#include <QSplitter>
#include <QStringList>
#include <QVBoxLayout>
#include <QtGlobal>

namespace {

/// How far a changed row's background is pulled off the pane's own base
/// colour. Enough to read the shape of the diff at a glance, faint enough that
/// the text on top keeps its contrast on a light and on a dark palette alike.
constexpr qreal kChangeWeight = 0.22;
/// The same, for the blank half of a one-sided row. Only enough to say "there
/// is nothing here", never enough to look like a change of its own.
constexpr qreal kAbsentWeight = 0.09;

/// How many characters the widest number in `numbers` needs.
int digitsFor(const QVector<int>& numbers) {
    int widest = 0;
    for (const int n : numbers) {
        widest = qMax(widest, n);
    }
    return widest == 0 ? 0 : QString::number(widest).size();
}

/// The gutter label for one block: the row's line number on that side, right
/// aligned in a fixed field, or that field's worth of blanks where the row does
/// not occupy the side.
QString gutterLabel(const QVector<int>& numbers, int digits, int block) {
    const int n = block >= 0 && block < numbers.size() ? numbers.at(block) : 0;
    return n <= 0 ? QString(digits, QLatin1Char(' '))
                  : QString::number(n).rightJustified(digits, QLatin1Char(' '));
}

} // namespace

DiffWidget::DiffWidget(DiffDocument* doc, QWidget* parent) : QWidget(parent), m_doc(doc) {
    if (doc != nullptr) {
        // The document is the tab's, not the area's: closing the tab must end
        // whatever it still has in flight.
        doc->setParent(this);
    }

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    auto* header = new QWidget(this);
    auto* headerLayout = new QHBoxLayout(header);
    headerLayout->setContentsMargins(6, 3, 6, 3);
    headerLayout->setSpacing(12);
    m_path = new QLabel(header);
    QFont pathFont = m_path->font();
    pathFont.setBold(true);
    m_path->setFont(pathFont);
    m_stat = new QLabel(header);
    m_notice = new QLabel(header);
    m_notice->hide();
    // A path and a daemon error are text, not markup: an angle bracket in
    // either would otherwise be parsed away. Only the stat is rich text, and
    // this widget composes every character of it.
    m_path->setTextFormat(Qt::PlainText);
    m_notice->setTextFormat(Qt::PlainText);
    m_stat->setTextFormat(Qt::RichText);
    headerLayout->addWidget(m_path);
    headerLayout->addWidget(m_stat);
    headerLayout->addWidget(m_notice);
    headerLayout->addStretch(1);
    layout->addWidget(header);

    auto* splitter = new QSplitter(Qt::Horizontal, this);
    splitter->setChildrenCollapsible(false);
    m_left = new CodeView(splitter);
    m_right = new CodeView(splitter);
    m_left->setReadOnly(true);
    m_right->setReadOnly(true);
    splitter->addWidget(m_left);
    splitter->addWidget(m_right);
    splitter->setStretchFactor(0, 1);
    splitter->setStretchFactor(1, 1);
    layout->addWidget(splitter, 1);

    // The same highlighter both sides, over the two halves of the document's
    // own tree-sitter pass. It maps character columns onto UTF-16 units, which
    // is the reason not to colour the spans here by hand. Each one parents
    // itself to the document it colours, so neither is held here: nothing
    // re-runs them by hand, and replacing the text re-runs them by itself.
    new RustHighlighter(m_left->document(), [this](int block) {
        if (m_doc.isNull() || block < 0 || block >= m_leftNo.size()) {
            return QStringLiteral("[]");
        }
        return m_doc->spansForLeftLine(m_leftNo.at(block));
    });
    new RustHighlighter(m_right->document(), [this](int block) {
        if (m_doc.isNull() || block < 0 || block >= m_rightNo.size()) {
            return QStringLiteral("[]");
        }
        return m_doc->spansForRightLine(m_rightNo.at(block));
    });

    // One row is one block on both sides, so a value on one scrollbar means the
    // same row on the other and no measuring is needed. Horizontally the two
    // panes share a font and a tab stop, so a pixel means the same thing in
    // both.
    link(m_left->verticalScrollBar(), m_right->verticalScrollBar(), &m_syncingVertical);
    link(m_right->verticalScrollBar(), m_left->verticalScrollBar(), &m_syncingVertical);
    link(m_left->horizontalScrollBar(), m_right->horizontalScrollBar(), &m_syncingHorizontal);
    link(m_right->horizontalScrollBar(), m_left->horizontalScrollBar(), &m_syncingHorizontal);

    updateHeader();
    if (doc == nullptr) {
        return;
    }
    // Set before `load`, so the first highlight pass already uses the right
    // palette. The pane's own colours are the only theme signal there is.
    doc->setDarkTheme(theme::isDark(palette()));

    QObject::connect(doc, &DiffDocument::rowsLoaded, this, &DiffWidget::onRowsLoaded);
    QObject::connect(doc, &DiffDocument::loadFailed, this, &DiffWidget::onLoadFailed);
    // The header is a function of the document, so every part of it that can
    // move republishes it.
    QObject::connect(doc, &DiffDocument::pathChanged, this, &DiffWidget::updateHeader);
    QObject::connect(doc, &DiffDocument::additionsChanged, this, &DiffWidget::updateHeader);
    QObject::connect(doc, &DiffDocument::deletionsChanged, this, &DiffWidget::updateHeader);
    QObject::connect(doc, &DiffDocument::truncatedChanged, this, &DiffWidget::updateHeader);
}

void DiffWidget::link(QScrollBar* from, QScrollBar* to, bool* guard) {
    QObject::connect(from, &QScrollBar::valueChanged, this, [to, guard](int value) {
        // Without the guard the answering `setValue` comes straight back, and a
        // value one side cannot reach (a shorter horizontal range) would ring
        // between the two. The guard is per axis: `QPlainTextEdit` lays blocks
        // out lazily, so scrolling one pane vertically can re-measure its
        // longest visible line and clamp its horizontal value in the same call.
        // One flag for both axes would swallow that as an echo and leave the
        // two sides on different columns.
        if (*guard) {
            return;
        }
        *guard = true;
        to->setValue(value);
        *guard = false;
    });
}

void DiffWidget::onRowsLoaded() {
    if (m_doc.isNull()) {
        return;
    }
    m_loadError.clear();
    const QJsonArray rows = QJsonDocument::fromJson(m_doc->rowsJson().toUtf8()).array();

    const QColor base = palette().base().color();
    const QColor addedTint = codeview::wash(base, theme::added(), kChangeWeight);
    const QColor removedTint = codeview::wash(base, theme::removed(), kChangeWeight);
    const QColor changedTint = codeview::wash(base, theme::changed(), kChangeWeight);
    const QColor absentTint = codeview::wash(base, palette().text().color(), kAbsentWeight);

    QStringList left;
    QStringList right;
    QVector<QColor> leftTints;
    QVector<QColor> rightTints;
    m_leftNo.clear();
    m_rightNo.clear();
    left.reserve(rows.size());
    right.reserve(rows.size());
    leftTints.reserve(rows.size());
    rightTints.reserve(rows.size());
    m_leftNo.reserve(rows.size());
    m_rightNo.reserve(rows.size());

    for (const QJsonValue value : rows) {
        const QJsonObject row = value.toObject();
        left.append(row.value(QStringLiteral("left_text")).toString());
        right.append(row.value(QStringLiteral("right_text")).toString());
        // Null on the side a row does not occupy, which `toInt` answers with
        // its default; the document reads that as "no line" too.
        m_leftNo.append(row.value(QStringLiteral("left_no")).toInt(0));
        m_rightNo.append(row.value(QStringLiteral("right_no")).toInt(0));

        const QString kind = row.value(QStringLiteral("kind")).toString();
        if (kind == QLatin1String("insert")) {
            leftTints.append(absentTint);
            rightTints.append(addedTint);
        } else if (kind == QLatin1String("delete")) {
            leftTints.append(removedTint);
            rightTints.append(absentTint);
        } else if (kind == QLatin1String("replace")) {
            leftTints.append(changedTint);
            rightTints.append(changedTint);
        } else {
            // Equal, and anything a later daemon adds: an invalid colour is how
            // `CodeView` is told to leave a row alone.
            leftTints.append(QColor());
            rightTints.append(QColor());
        }
    }

    m_leftDigits = digitsFor(m_leftNo);
    m_rightDigits = digitsFor(m_rightNo);
    m_left->setPlainText(left.join(QLatin1Char('\n')));
    m_right->setPlainText(right.join(QLatin1Char('\n')));
    m_left->setRowTints(leftTints);
    m_right->setRowTints(rightTints);
    // Re-installed rather than left from the constructor: this is what makes
    // the gutter measure itself again, and the numbers change even on a reload
    // that happens to have the same number of rows.
    m_left->setLineNumberProvider(
        [this](int block) { return gutterLabel(m_leftNo, m_leftDigits, block); });
    m_right->setLineNumberProvider(
        [this](int block) { return gutterLabel(m_rightNo, m_rightDigits, block); });

    updateHeader();
}

void DiffWidget::onLoadFailed(const QString& message) {
    m_loadError = message;
    updateHeader();
}

void DiffWidget::updateHeader() {
    if (m_doc.isNull()) {
        return;
    }
    m_path->setText(m_doc->getPath());
    m_path->setToolTip(m_doc->getWorkspaceId() + QLatin1Char(':') + m_doc->getPath());

    const bool dark = theme::isDark(palette());
    // U+2212, the minus sign, written as a code point: the file is compiled
    // without a byte order mark and MSVC would read a literal as the ANSI code
    // page.
    const QString minus(QChar(0x2212));
    m_stat->setText(QStringLiteral("<span style=\"color:%1\">+%2</span> "
                                   "<span style=\"color:%3\">%4%5</span>")
                        .arg(theme::ink(theme::added(), dark).name(),
                             QString::number(m_doc->getAdditions()),
                             theme::ink(theme::removed(), dark).name(), minus,
                             QString::number(m_doc->getDeletions())));

    QString notice = m_loadError;
    QColor colour = theme::ink(theme::removed(), dark);
    if (notice.isEmpty() && m_doc->getTruncated()) {
        notice = QStringLiteral("diff truncated (time budget)");
        colour = theme::ink(theme::changed(), dark);
    }
    QPalette noticePalette = m_notice->palette();
    noticePalette.setColor(QPalette::WindowText, colour);
    m_notice->setPalette(noticePalette);
    m_notice->setText(notice);
    m_notice->setToolTip(notice);
    m_notice->setVisible(!notice.isEmpty());
}
