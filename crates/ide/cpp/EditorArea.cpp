#include "EditorArea.h"
#include "CodeView.h"
#include "DiffWidget.h"
#include "EditorWidget.h"
#include "bondsymphonic-ide/src/qobjects/diff_document.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/editor_document.cxxqt.h"
#include <QLabel>
#include <QLatin1Char>
#include <QMessageBox>
#include <QStackedWidget>
#include <QStringList>
#include <QTabWidget>
#include <QVBoxLayout>
#include <QtGlobal>
#include <utility>

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

/// The workspace a tab key names, empty for a page that has no key. Reading it
/// back off the key is what lets a workspace be closed without knowing what
/// kinds of page it has open.
QString workspaceOfKey(const QString& key) {
    const QStringList parts = key.split(QLatin1Char('\n'));
    return parts.size() == 3 ? parts.at(1) : QString();
}

} // namespace

EditorArea::EditorArea(QWidget* parent) : QWidget(parent) {
    m_ask = [this](const QString& title) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle(QStringLiteral("Unsaved changes"));
        box.setText(QStringLiteral("Save changes to %1 before closing?").arg(title));
        box.setStandardButtons(QMessageBox::Save | QMessageBox::Discard | QMessageBox::Cancel);
        box.setDefaultButton(QMessageBox::Save);
        switch (box.exec()) {
        case QMessageBox::Save:
            return Unsaved::Save;
        case QMessageBox::Discard:
            return Unsaved::Discard;
        default:
            return Unsaved::Cancel;
        }
    };

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
        if (auto* open = qobject_cast<EditorWidget*>(m_tabs->widget(existing))) {
            open->view()->setFocus();
        }
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
    // A file you just opened takes the caret. Without this the focus stays in
    // whatever asked for it -- the Explorer tree, usually -- and the window's
    // Edit menu has nothing to act on.
    editor->view()->setFocus();
    // Last: `open` is asynchronous, and the pane has to be wired up before its
    // answer arrives.
    doc->open(workspaceId, path);
}

void EditorArea::openDiff(const QString& workspaceId, const QString& path) {
    if (workspaceId.isEmpty() || path.isEmpty()) {
        return;
    }
    // A different kind from the file tab on the same path, so opening a diff
    // does not take the place of the file the user is editing.
    const QString key = tabKey(QStringLiteral("diff"), workspaceId, path);
    const int existing = indexOfKey(key);
    if (existing >= 0) {
        m_tabs->setCurrentIndex(existing);
        m_stack->setCurrentWidget(m_tabs);
        return;
    }

    auto* doc = new DiffDocument(this);
    auto* widget = new DiffWidget(doc, this);
    const QString title = path.section(QLatin1Char('/'), -1) + QStringLiteral(" (diff)");
    widget->setProperty(kTabKey, key);
    widget->setProperty(kTabTitle, title);

    const int index = m_tabs->addTab(widget, title);
    m_tabs->setTabToolTip(index, workspaceId + QLatin1Char(':') + path);
    m_tabs->setCurrentIndex(index);
    m_stack->setCurrentWidget(m_tabs);
    // Last: `load` is asynchronous, and the pane has to be wired up before its
    // answer arrives.
    doc->load(workspaceId, path);
}

void EditorArea::setUnsavedPrompt(std::function<Unsaved(const QString&)> ask) {
    if (ask) {
        m_ask = std::move(ask);
    }
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
    if (workspaceId.isEmpty()) {
        return;
    }
    // Backwards: removing a tab renumbers everything after it.
    for (int i = m_tabs->count() - 1; i >= 0; --i) {
        QWidget* page = m_tabs->widget(i);
        // By the key rather than by the page's document, so a diff tab on a
        // dead workspace goes with the editors rather than being left showing a
        // worktree that no longer exists.
        if (workspaceOfKey(page->property(kTabKey).toString()) != workspaceId) {
            continue;
        }
        removePage(page);
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
        // Asked once. A second close request while the write is out would only
        // put the same question up again over a tab that is already leaving.
        if (m_closing.contains(page)) {
            return false;
        }
        const Unsaved answer = m_ask(page->property(kTabTitle).toString());
        if (answer == Unsaved::Cancel) {
            return false;
        }
        if (answer == Unsaved::Save) {
            PendingClose pending;
            pending.saved = QObject::connect(doc, &EditorDocument::saved, this,
                                             [this, page] { onSavedForClose(page); });
            // A save that failed leaves the tab and its edits alone, and takes
            // the close with it: without this the connection would sit armed
            // and a save ten minutes later would close the tab by itself.
            pending.failed = QObject::connect(doc, &EditorDocument::saveFailed, this,
                                              [this, page](const QString&) { abandonClose(page); });
            m_closing.insert(page, pending);
            editor->save();
            return false;
        }
    }
    removePage(page);
    return true;
}

void EditorArea::onSavedForClose(QWidget* page) {
    auto* editor = qobject_cast<EditorWidget*>(page);
    EditorDocument* doc = editor == nullptr ? nullptr : editor->document();
    // `saved` does not mean the buffer is clean: a keystroke that landed while
    // the write was in flight leaves the file on disk older than the pane, and
    // closing now would throw that edit away.
    if (doc != nullptr && doc->getDirty()) {
        abandonClose(page);
        return;
    }
    abandonClose(page);
    removePage(page);
}

void EditorArea::abandonClose(QWidget* page) {
    const auto it = m_closing.constFind(page);
    if (it == m_closing.constEnd()) {
        return;
    }
    QObject::disconnect(it->saved);
    QObject::disconnect(it->failed);
    m_closing.remove(page);
}

void EditorArea::removePage(QWidget* page) {
    const int index = m_tabs->indexOf(page);
    if (index < 0) {
        return;
    }
    // Before the page goes: the entry is keyed by a pointer that is about to
    // name nothing.
    abandonClose(page);
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
