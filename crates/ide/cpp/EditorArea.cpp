#include "EditorArea.h"
#include "CodeView.h"
#include "DiffWidget.h"
#include "EditorWidget.h"
#include "bondsymphonic-ide/src/qobjects/diff_document.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/editor_document.cxxqt.h"
#include <QApplication>
#include <QCoreApplication>
#include <QEvent>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLatin1Char>
#include <QMessageBox>
#include <QPointer>
#include <QStackedWidget>
#include <QTabBar>
#include <QTabWidget>
#include <QVBoxLayout>
#include <QtGlobal>
#include <cstdint>
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

/// The page's document when it is an editor with unsaved edits, else null.
/// Six callers ask the same three-part question -- is it an editor, does it
/// have a document, is that document dirty -- and getting one part of it wrong
/// is how a dirty editor gets closed without a prompt.
EditorDocument* dirtyDocument(QWidget* page) {
    auto* editor = qobject_cast<EditorWidget*>(page);
    EditorDocument* doc = editor == nullptr ? nullptr : editor->document();
    return (doc != nullptr && doc->getDirty()) ? doc : nullptr;
}

/// The kind a tab key names ("file" or "diff"), empty for a page with no key.
QString kindOfKey(const QString& key) {
    return key.section(QLatin1Char('\n'), 0, 0);
}

/// The path a tab key names, empty for a page with no key. Everything from the
/// third field on, so a path containing a newline comes back whole.
QString pathOfKey(const QString& key) {
    return key.section(QLatin1Char('\n'), 2);
}

/// The workspace a tab key names, empty for a page that has no key. Reading it
/// back off the key is what lets a workspace be closed without knowing what
/// kinds of page it has open.
QString workspaceOfKey(const QString& key) {
    // The second field, however many follow it: a path may contain a newline,
    // and a key counted field by field would then have four and be skipped,
    // leaving that tab open over a workspace that no longer exists. The kind and
    // the workspace id are both newline-free, so the second field is the id
    // whatever the path looks like.
    return key.section(QLatin1Char('\n'), 1, 1);
}

} // namespace

EditorArea::EditorArea(QWidget* parent) : QWidget(parent) {
    m_ask = [this](const QString& title, bool closingAll) {
        // `SaveAll` rather than `Save` for the whole window, so the button says
        // what pressing it does: every dirty editor is written, not just the one
        // in front.
        const QMessageBox::StandardButton accept =
            closingAll ? QMessageBox::SaveAll : QMessageBox::Save;
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle(QStringLiteral("Unsaved changes"));
        box.setText(QStringLiteral("Save changes to %1 before closing?").arg(title));
        box.setStandardButtons(accept | QMessageBox::Discard | QMessageBox::Cancel);
        box.setDefaultButton(accept);
        switch (box.exec()) {
        case QMessageBox::Save:
        case QMessageBox::SaveAll:
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
    // Dragging a tab changes the order the files come back in next time.
    QObject::connect(m_tabs->tabBar(), &QTabBar::tabMoved, this,
                     [this](int, int) { emit openEditorsChanged(); });
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
    QObject::connect(doc, &EditorDocument::dirtyChanged, this, [this, editor] {
        updateTabTitle(editor);
        emit unsavedStateChanged();
    });
    // Carried up for the window: a close waiting on a save-all has to stop
    // waiting when one of the writes comes back a failure.
    QObject::connect(doc, &EditorDocument::saveFailed, this,
                     [this](const QString& message) { emit saveFailed(message); });
    m_tabs->setCurrentIndex(index);
    m_stack->setCurrentWidget(m_tabs);
    // A file you just opened takes the caret. Without this the focus stays in
    // whatever asked for it -- the Explorer tree, usually -- and the window's
    // Edit menu has nothing to act on.
    editor->view()->setFocus();
    // Last: `open` is asynchronous, and the pane has to be wired up before its
    // answer arrives.
    doc->open(workspaceId, path);
    emit openEditorsChanged();
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

bool EditorArea::hasUnsavedEditors() const {
    for (int i = 0; i < m_tabs->count(); ++i) {
        if (dirtyDocument(m_tabs->widget(i)) != nullptr) {
            return true;
        }
    }
    return false;
}

EditorArea::Unsaved EditorArea::askUnsavedAll() {
    QString title;
    int dirty = 0;
    for (int i = 0; i < m_tabs->count(); ++i) {
        QWidget* page = m_tabs->widget(i);
        if (dirtyDocument(page) == nullptr) {
            continue;
        }
        if (dirty == 0) {
            title = page->property(kTabTitle).toString();
        }
        ++dirty;
    }
    // Nothing to lose, so nothing to ask. `Discard` and not `Save`: a save-all
    // here would arm a wait for writes that are never going to happen.
    if (dirty == 0) {
        return Unsaved::Discard;
    }
    if (dirty > 1) {
        title = QStringLiteral("%1 files").arg(dirty);
    }
    return m_ask(title, true);
}

void EditorArea::setUnsavedPrompt(std::function<Unsaved(const QString&, bool)> ask) {
    if (ask) {
        m_ask = std::move(ask);
    }
}

EditorWidget* EditorArea::currentEditor() const {
    return qobject_cast<EditorWidget*>(m_tabs->currentWidget());
}

void EditorArea::saveAll() {
    for (int i = 0; i < m_tabs->count(); ++i) {
        QWidget* page = m_tabs->widget(i);
        // A page with a dirty document is an `EditorWidget` by construction:
        // that is the only kind of page `dirtyDocument` answers for.
        if (dirtyDocument(page) != nullptr) {
            qobject_cast<EditorWidget*>(page)->save();
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
    // Guarded rather than raw, because the question below runs a nested event
    // loop and the page can be destroyed inside it.
    QPointer<QWidget> page = m_tabs->widget(index);
    if (page.isNull()) {
        return false;
    }
    if (dirtyDocument(page) != nullptr) {
        // Asked once. A second close request while the write is out would only
        // put the same question up again over a tab that is already leaving.
        if (m_closing.contains(page)) {
            return false;
        }
        const Unsaved answer = m_ask(page->property(kTabTitle).toString(), false);
        // The modal ran an event loop of its own, and everything the daemon
        // had to say arrived in it. Destroying the workspace this tab belongs
        // to closes its tabs, which takes the page, the editor inside it and
        // that editor's document; the answer then comes back to a `page` that
        // names freed memory. Nothing read before the question survives it, so
        // every one of them is derived again here.
        if (page.isNull() || m_tabs->indexOf(page) < 0) {
            // Gone, by a route that did not come through this call. There is
            // no tab left to save, to close, or to arm a close on -- an entry
            // in `m_closing` keyed by this page would never be taken out
            // again, and the pointer keying it names memory the next tab may
            // be handed.
            return true;
        }
        if (answer == Unsaved::Cancel) {
            return false;
        }
        EditorDocument* doc = dirtyDocument(page);
        if (answer == Unsaved::Save && doc != nullptr) {
            QWidget* const alive = page.data();
            PendingClose pending;
            pending.saved = QObject::connect(doc, &EditorDocument::saved, this,
                                             [this, alive] { onSavedForClose(alive); });
            // A save that failed leaves the tab and its edits alone, and takes
            // the close with it: without this the connection would sit armed
            // and a save ten minutes later would close the tab by itself.
            pending.failed =
                QObject::connect(doc, &EditorDocument::saveFailed, this,
                                 [this, alive](const QString&) { abandonClose(alive); });
            m_closing.insert(alive, pending);
            qobject_cast<EditorWidget*>(alive)->save();
            return false;
        }
        // `Save` with nothing dirty left to write falls through with `Discard`:
        // the write it would have waited for is never going to happen, and
        // there is nothing in the buffer to lose.
    }
    removePage(page);
    return true;
}

int EditorArea::pendingCloseCount() const {
    return static_cast<int>(m_closing.size());
}

void EditorArea::onSavedForClose(QWidget* page) {
    // `saved` does not mean the buffer is clean: a keystroke that landed while
    // the write was in flight leaves the file on disk older than the pane, and
    // closing now would throw that edit away.
    if (dirtyDocument(page) != nullptr) {
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
    // A dirty editor can leave this way -- `Discard`, or a workspace that is
    // gone -- so what the window is waiting on may have just become nothing.
    emit unsavedStateChanged();
    emit openEditorsChanged();
}

void EditorArea::updateTabTitle(QWidget* page) {
    const int index = m_tabs->indexOf(page);
    if (index < 0) {
        return;
    }
    const bool dirty = dirtyDocument(page) != nullptr;
    const QString title = page->property(kTabTitle).toString();
    m_tabs->setTabText(index, dirty ? QString::fromUtf8(kDirtyMarker) + title : title);
}

QString EditorArea::openEditorsJson() const {
    QJsonObject byWorkspace;
    const QString currentKey =
        m_tabs->currentWidget() == nullptr
            ? QString()
            : m_tabs->currentWidget()->property(kTabKey).toString();
    for (int i = 0; i < m_tabs->count(); ++i) {
        const QString key = m_tabs->widget(i)->property(kTabKey).toString();
        if (kindOfKey(key) != QLatin1String("file")) {
            continue;
        }
        const QString workspaceId = workspaceOfKey(key);
        const QString path = pathOfKey(key);
        if (workspaceId.isEmpty() || path.isEmpty()) {
            continue;
        }
        QJsonObject entry = byWorkspace.value(workspaceId).toObject();
        QJsonArray open = entry.value(QStringLiteral("open")).toArray();
        open.append(path);
        entry.insert(QStringLiteral("open"), open);
        if (key == currentKey) {
            entry.insert(QStringLiteral("active"), path);
        }
        byWorkspace.insert(workspaceId, entry);
    }
    return QString::fromUtf8(QJsonDocument(byWorkspace).toJson(QJsonDocument::Compact));
}

int EditorArea::indexOfKey(const QString& key) const {
    for (int i = 0; i < m_tabs->count(); ++i) {
        if (m_tabs->widget(i)->property(kTabKey).toString() == key) {
            return i;
        }
    }
    return -1;
}

// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// widgets, leaks a QApplication and asserts -- and a shipped IDE has no caller
// for any of it. `build.rs` defines `BS_WIDGET_TESTS` for every profile but
// `release`, which is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
//
// The IDE's shell is C++ and its test suites are Rust, so the checks that need
// a live widget are written here, beside the widget they are about, and
// exported as plain C entry points that `crates/ide/tests/qobject_smoke.rs`
// calls. Each answers 0 for a pass and a small non-zero code naming the
// assertion that failed, which is all that has to cross the boundary.

extern "C" void bs_widget_test_begin() {
    if (QCoreApplication::instance() != nullptr) {
        return;
    }
    // Offscreen unconditionally: the suite must never put a window on the
    // developer's desktop, and no test here looks at a pixel.
    qputenv("QT_QPA_PLATFORM", "offscreen");
    static int argc = 1;
    static char name[] = "bs-widget-tests";
    static char* argv[] = { name, nullptr };
    // Deliberately leaked. Qt wants the application to outlive every widget,
    // and the test process ends with it.
    new QApplication(argc, argv);
}

/// A dirty tab closed while the workspace it belongs to is destroyed under the
/// prompt: the page, its editor and its document are all gone by the time the
/// answer comes back, and nothing may be asked of any of them afterwards.
extern "C" std::int32_t bs_widget_test_editor_area_survives_a_destroyed_workspace() {
    EditorArea area;
    // The area asks nothing of the daemon, so an unconnected process is
    // enough: the document's `open` reports that it has no connection and the
    // tab is otherwise a real one.
    area.openFile(QStringLiteral("ws_1"), QStringLiteral("a.txt"));
    EditorWidget* editor = area.currentEditor();
    if (editor == nullptr || editor->document() == nullptr) {
        return 1;
    }
    editor->document()->setDirty(true);
    QPointer<QWidget> page = editor;
    area.setUnsavedPrompt([&area](const QString&, bool) {
        // What a real modal's nested event loop does when the workspace goes
        // while the box is up: the tabs are removed and their deferred
        // deletion is delivered before the answer gets back to `closeTab`.
        area.closeWorkspace(QStringLiteral("ws_1"));
        QCoreApplication::sendPostedEvents(nullptr, QEvent::DeferredDelete);
        return EditorArea::Unsaved::Save;
    });
    // True: the question was answered, the tab is not there any more, and that
    // is what the answer reports. Which of the two routes took it away is not
    // something the caller can act on differently.
    if (!area.closeTab(0)) {
        return 2;
    }
    if (!page.isNull()) {
        return 3;
    }
    // The close must not have been armed on a page that no longer exists: that
    // entry would never be taken out again, and the pointer keying it names
    // memory the next tab may be handed.
    if (area.pendingCloseCount() != 0) {
        return 4;
    }
    return 0;
}
#endif // BS_WIDGET_TESTS
