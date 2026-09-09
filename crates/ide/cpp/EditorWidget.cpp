#include "EditorWidget.h"
#include "CodeView.h"
#include "RustHighlighter.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/editor_document.cxxqt.h"
#include <QChar>
#include <QColor>
#include <QFrame>
#include <QHBoxLayout>
#include <QLabel>
#include <QLatin1Char>
#include <QMessageBox>
#include <QPalette>
#include <QPushButton>
#include <QScrollBar>
#include <QTextCursor>
#include <QTextDocument>
#include <QVBoxLayout>
#include <QtGlobal>

EditorWidget::EditorWidget(EditorDocument* doc, QWidget* parent) : QWidget(parent), m_doc(doc) {
    if (doc != nullptr) {
        // The document owns a file watch on the daemon; tying it to the pane is
        // what ends that watch when the tab closes.
        doc->setParent(this);
    }

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_notice = new QLabel(this);
    m_notice->setWordWrap(true);
    m_notice->setMargin(4);
    // What goes in here is a daemon error or a file path, both of them text.
    // Left at `AutoText` an error containing angle brackets would be guessed to
    // be HTML and rendered as markup, which is what `DiffWidget` sets
    // `PlainText` on its own notice to avoid.
    m_notice->setTextFormat(Qt::PlainText);
    m_notice->setAutoFillBackground(true);
    QPalette noticePalette = m_notice->palette();
    // Washed, not the raw role: `AlternateBase` is a saturated accent on some
    // palettes and a bar in that colour shouts over the file it is about.
    noticePalette.setColor(QPalette::Window,
                           codeview::wash(palette().base().color(),
                                          palette().alternateBase().color(),
                                          codeview::kWashAmount));
    m_notice->setPalette(noticePalette);
    m_notice->hide();
    layout->addWidget(m_notice);

    m_externalBar = new QFrame(this);
    m_externalBar->setFrameShape(QFrame::StyledPanel);
    m_externalBar->setAutoFillBackground(true);
    m_externalBar->setPalette(noticePalette);
    auto* barLayout = new QHBoxLayout(m_externalBar);
    barLayout->setContentsMargins(6, 2, 6, 2);
    barLayout->addWidget(new QLabel(QStringLiteral("File changed on disk"), m_externalBar));
    barLayout->addStretch(1);
    auto* reload = new QPushButton(QStringLiteral("Reload"), m_externalBar);
    auto* keep = new QPushButton(QStringLiteral("Keep mine"), m_externalBar);
    barLayout->addWidget(reload);
    barLayout->addWidget(keep);
    m_externalBar->hide();
    layout->addWidget(m_externalBar);

    m_view = new CodeView(this);
    layout->addWidget(m_view, 1);

    QObject::connect(m_view->document(), &QTextDocument::contentsChange, this,
                     &EditorWidget::onContentsChange);
    // After that connection, and the order is load-bearing. Both slots run on
    // one `contentsChange`, in the order they were made: ours first, so
    // `applyEdit` has updated the buffer before the highlighter asks it for the
    // spans of the block that just changed. Attached first, the highlighter
    // would colour every keystroke from the state before it.
    m_highlighter = new RustHighlighter(m_view->document(), [this](int block) {
        return m_doc.isNull() ? QString() : m_doc->spansForLine(block);
    });

    if (doc == nullptr) {
        return;
    }
    // Set before `open`, so the first highlight pass already uses the right
    // palette. The pane's own colours are the only theme signal there is.
    doc->setDarkTheme(theme::isDark(palette()));

    QObject::connect(doc, &EditorDocument::loaded, this, &EditorWidget::onLoaded);
    QObject::connect(doc, &EditorDocument::readOnlyReasonChanged, this, &EditorWidget::updateNotice);
    QObject::connect(doc, &EditorDocument::highlightChanged, this,
                     [this](::std::int32_t from, ::std::int32_t to) {
                         m_highlighter->rehighlightLines(from, to);
                     });
    QObject::connect(doc, &EditorDocument::externalChange, this, [this] { m_externalBar->show(); });
    QObject::connect(doc, &EditorDocument::loadFailed, this, [this](const QString& message) {
        m_loadError = message;
        updateNotice();
    });
    QObject::connect(doc, &EditorDocument::saveFailed, this, [this](const QString& message) {
        QMessageBox::warning(this, QStringLiteral("Save failed"), message);
    });
    // Hidden as soon as the answer is given rather than when the reload lands:
    // a re-read that fails would otherwise leave the bar up with both its
    // buttons already spent.
    QObject::connect(reload, &QPushButton::clicked, this, [this] {
        m_externalBar->hide();
        if (!m_doc.isNull()) {
            m_doc->acceptExternal();
        }
    });
    QObject::connect(keep, &QPushButton::clicked, this, [this] {
        m_externalBar->hide();
        if (!m_doc.isNull()) {
            m_doc->keepLocal();
        }
    });
}

EditorDocument* EditorWidget::document() const {
    return m_doc.data();
}

CodeView* EditorWidget::view() const {
    return m_view;
}

void EditorWidget::save() {
    if (!m_doc.isNull()) {
        m_doc->save();
    }
}

void EditorWidget::onLoaded() {
    if (m_doc.isNull()) {
        return;
    }
    // A silent reload replaces the whole buffer under the caret; without these
    // the pane would jump to the top every time an agent touched the file.
    const int caret = m_view->textCursor().position();
    const int scroll = m_view->verticalScrollBar()->value();

    m_settingText = true;
    m_view->setPlainText(m_doc->text());
    m_settingText = false;

    QTextCursor cursor = m_view->textCursor();
    cursor.setPosition(qBound(0, caret, m_view->document()->characterCount() - 1));
    m_view->setTextCursor(cursor);
    m_view->verticalScrollBar()->setValue(scroll);

    m_loadError.clear();
    updateNotice();
    m_externalBar->hide();
    m_highlighter->rehighlight();
}

void EditorWidget::onContentsChange(int position, int charsRemoved, int charsAdded) {
    if (m_settingText || m_doc.isNull()) {
        return;
    }
    // The highlighter reports its own format-only changes through this signal
    // with nothing added or removed. Passing one on would mark the file dirty
    // the moment it was coloured.
    if (charsRemoved == 0 && charsAdded == 0) {
        return;
    }
    QTextCursor cursor(m_view->document());
    cursor.setPosition(position);
    cursor.setPosition(position + charsAdded, QTextCursor::KeepAnchor);
    QString inserted = cursor.selectedText();
    // `selectedText` writes a paragraph separator where the document has a
    // block boundary; the buffer on the other side counts newlines.
    inserted.replace(QChar(QChar::ParagraphSeparator), QLatin1Char('\n'));
    m_doc->applyEdit(position, charsRemoved, inserted);
}

void EditorWidget::updateNotice() {
    const QString reason = m_doc.isNull() ? QString() : m_doc->getReadOnlyReason();
    // The view keeps its own caret and current-line band in step off the
    // `ReadOnlyChange` event this sends.
    m_view->setReadOnly(!reason.isEmpty());
    const QString text = m_loadError.isEmpty() ? reason : m_loadError;
    m_notice->setText(text);
    m_notice->setVisible(!text.isEmpty());
}
