#include "EditorArea.h"
#include "EditorWidget.h"
#include "bondsymphonic-ide/src/qobjects/editor_document.cxxqt.h"
#include <QLabel>
#include <QLatin1Char>
#include <QMessageBox>
#include <QStackedWidget>
#include <QTabWidget>
#include <QVBoxLayout>
#include <QtGlobal>

namespace {

/// What a tab shows, so re-opening the same thing activates it instead of
/// stacking another copy. The kind is part of the key: a file and its diff are
/// two tabs on one path.
const char* const kTabKey = "bsTabKey";
/// The tab's file name without the dirty marker, so the marker can be put back
/// and taken away without re-deriving the title.
const char* const kTabTitle = "bsTabTitle";
/// In front of the file name while the editor has unsaved edits.
const char* const kDirtyMarker = "\xe2\x97\x8f ";

QString tabKey(const QString& kind, const QString& workspaceId, const QString& path) {
    return kind + QLatin1Char('\n') + workspaceId + QLatin1Char('\n') + path;
}

} // namespace

EditorArea::EditorArea(QWidget* parent) : QWidget(parent) {
    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_stack = new QStackedWidget(this);
    m_placeholder = new QLabel(QStringLiteral("Open a file from the Explorer"), m_stack);
    m_placeholder->setAlignment(Qt::AlignCenter);
    m_placeholder->setEnabled(false);
    m_tabs = new QTabWidget(m_stack);
    m_tabs->setTabsClosable(true);
    m_tabs->setMovable(true);
    m_tabs->setDocumentMode(true);
    m_stack->addWidget(m_placeholder);
    m_stack->addWidget(m_tabs);
    layout->addWidget(m_stack);

    QObject::connect(m_tabs, &QTabWidget::tabCloseRequested, this,
                     [this](int index) { closeTab(index); });
    QObject::connect(m_tabs, &QTabWidget::currentChanged, this,
                     [this](int) { emit currentEditorChanged(currentEditor()); });
}

void EditorArea::openFile(const QString& workspaceId, const QString& path) {
    if (workspaceId.isEmpty() || path.isEmpty()) {
        return;
    }
    const QString key = tabKey(QStringLiteral("file"), workspaceId, path);
    const int existing = indexOfKey(key);
    if (existing >= 0) {
        m_tabs->setCurrentIndex(existing);
        m_stack->setCurrentWidget(m_tabs);
        return;
    }

    auto* doc = new EditorDocument(this);
    auto* editor = new EditorWidget(doc, this);
    const QString title = path.section(QLatin1Char('/'), -1);
    editor->setProperty(kTabKey, key);
    editor->setProperty(kTabTitle, title);

    const int index = m_tabs->addTab(editor, title);
    m_tabs->setTabToolTip(index, workspaceId + QLatin1Char(':') + path);
    QObject::connect(doc, &EditorDocument::dirtyChanged, this,
                     [this, editor] { updateTabTitle(editor); });
    m_tabs->setCurrentIndex(index);
    m_stack->setCurrentWidget(m_tabs);
    // Last: `open` is asynchronous, and the pane has to be wired up before its
    // answer arrives.
    doc->open(workspaceId, path);
}

void EditorArea::openDiff(const QString& workspaceId, const QString& path) {
    qWarning("EditorArea::openDiff: diff view not yet available (%s:%s)",
             qUtf8Printable(workspaceId), qUtf8Printable(path));
}

EditorWidget* EditorArea::currentEditor() const {
    return qobject_cast<EditorWidget*>(m_tabs->currentWidget());
}

void EditorArea::saveAll() {
    for (int i = 0; i < m_tabs->count(); ++i) {
        auto* editor = qobject_cast<EditorWidget*>(m_tabs->widget(i));
        if (editor != nullptr && editor->document() != nullptr && editor->document()->getDirty()) {
            editor->save();
        }
    }
}

void EditorArea::closeWorkspace(const QString& workspaceId) {
    // Backwards: removing a tab renumbers everything after it.
    for (int i = m_tabs->count() - 1; i >= 0; --i) {
        auto* editor = qobject_cast<EditorWidget*>(m_tabs->widget(i));
        if (editor == nullptr || editor->document() == nullptr) {
            continue;
        }
        if (editor->document()->getWorkspaceId() != workspaceId) {
            continue;
        }
        removePage(editor);
    }
}

bool EditorArea::closeTab(int index) {
    QWidget* page = m_tabs->widget(index);
    if (page == nullptr) {
        return false;
    }
    auto* editor = qobject_cast<EditorWidget*>(page);
    EditorDocument* doc = editor == nullptr ? nullptr : editor->document();
    if (doc != nullptr && doc->getDirty()) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle(QStringLiteral("Unsaved changes"));
        box.setText(QStringLiteral("Save changes to %1 before closing?")
                        .arg(page->property(kTabTitle).toString()));
        box.setStandardButtons(QMessageBox::Save | QMessageBox::Discard | QMessageBox::Cancel);
        box.setDefaultButton(QMessageBox::Save);
        const int answer = box.exec();
        if (answer == QMessageBox::Cancel) {
            return false;
        }
        if (answer == QMessageBox::Save) {
            // The tab goes when the daemon confirms the write. A failed save
            // keeps it open with its edits, and the pane's own message box says
            // why.
            QObject::connect(
                doc, &EditorDocument::saved, this, [this, page] { removePage(page); },
                Qt::SingleShotConnection);
            editor->save();
            return false;
        }
    }
    removePage(page);
    return true;
}

void EditorArea::removePage(QWidget* page) {
    const int index = m_tabs->indexOf(page);
    if (index < 0) {
        return;
    }
    // `removeTab` publishes the new current tab, which is what tells the window
    // its Edit actions have moved on.
    m_tabs->removeTab(index);
    page->deleteLater();
    if (m_tabs->count() == 0) {
        m_stack->setCurrentWidget(m_placeholder);
    }
}

void EditorArea::updateTabTitle(QWidget* page) {
    const int index = m_tabs->indexOf(page);
    if (index < 0) {
        return;
    }
    auto* editor = qobject_cast<EditorWidget*>(page);
    const bool dirty =
        editor != nullptr && editor->document() != nullptr && editor->document()->getDirty();
    const QString title = page->property(kTabTitle).toString();
    m_tabs->setTabText(index, dirty ? QString::fromUtf8(kDirtyMarker) + title : title);
}

int EditorArea::indexOfKey(const QString& key) const {
    for (int i = 0; i < m_tabs->count(); ++i) {
        if (m_tabs->widget(i)->property(kTabKey).toString() == key) {
            return i;
        }
    }
    return -1;
}
