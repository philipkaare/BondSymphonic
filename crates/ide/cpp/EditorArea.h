#pragma once
#include <QHash>
#include <QMetaObject>
#include <QString>
#include <QWidget>
#include <functional>

class EditorWidget;
class QLabel;
class QStackedWidget;
class QTabWidget;

/// The centre pane: a tab per open file, behind a placeholder while none is
/// open.
///
/// A tab is identified by what it shows, not by its position, so the same file
/// in the same workspace is only ever opened once however the tabs are
/// rearranged. The area asks nothing of the daemon itself: it creates one
/// `EditorDocument` per tab, hands it to an `EditorWidget`, and lets the pane
/// and the document settle everything between them.
class EditorArea : public QWidget {
    Q_OBJECT
public:
    explicit EditorArea(QWidget* parent = nullptr);

    /// Shows `path` of `workspaceId`, activating the tab that already has it or
    /// opening a new one.
    void openFile(const QString& workspaceId, const QString& path);

    /// Shows `path`'s working copy against its base, side by side, activating
    /// the tab that already has it or opening a new one. A diff is its own kind
    /// of tab, so it sits beside the file's editor rather than replacing it.
    void openDiff(const QString& workspaceId, const QString& path);

    /// The editor in the current tab, or null when there is none or the tab
    /// holds something that is not an editor, a diff among them.
    EditorWidget* currentEditor() const;

    /// Saves every dirty editor. Clean ones are left alone, so a save-all does
    /// not rewrite files nothing has touched.
    void saveAll();

    /// Closes every tab belonging to `workspaceId`, of whatever kind,
    /// discarding unsaved edits. Used when the workspace itself is gone and
    /// there is nothing left to save to.
    void closeWorkspace(const QString& workspaceId);

    /// What to do with a dirty editor whose tab is closing.
    enum class Unsaved { Save, Discard, Cancel };

    /// Whether any open tab is an editor with unsaved edits. What the window
    /// asks before it lets itself be closed.
    bool hasUnsavedEditors() const;

    /// Asks the unsaved question once for the whole area, naming the file when
    /// there is one and counting them when there are several. Answers `Discard`
    /// without asking anything when nothing is dirty.
    Unsaved askUnsavedAll();

    /// Replaces the question `closeTab` and `askUnsavedAll` put to the user.
    /// The default puts a modal box in front of them; this is the seam that lets
    /// the answer come from somewhere else. `closingAll` is false for one tab
    /// and true for the whole window, which is the difference between a Save
    /// button and a Save All one.
    void setUnsavedPrompt(std::function<Unsaved(const QString& title, bool closingAll)> ask);

    /// Closes the tab at `index`, asking first when it has unsaved edits.
    /// Returns whether the tab is gone: a cancelled prompt and a save still in
    /// flight both answer false, and the save closes the tab once the write has
    /// landed and nothing has been typed since.
    bool closeTab(int index);

signals:
    /// The current tab changed. `editor` is null when no editor is showing.
    void currentEditorChanged(EditorWidget* editor);

    /// Some editor became dirty or clean, or a tab holding one went away, so
    /// the answer to `hasUnsavedEditors` may have changed. The window waits on
    /// this while a close-triggered save-all is in flight.
    void unsavedStateChanged();

    /// An editor's write failed. Carried up because a window that is waiting
    /// for saves to land has to stop waiting, and the pane has already told the
    /// user what went wrong.
    void saveFailed(const QString& message);

private:
    /// The two connections that close a tab once its save lands.
    struct PendingClose {
        QMetaObject::Connection saved;
        QMetaObject::Connection failed;
    };

    /// The `saved` that answers the close: closes the tab, unless something was
    /// typed while the write was out.
    void onSavedForClose(QWidget* page);
    /// Forgets the pending close for `page`, if there is one. Both the failure
    /// path and the "typed during the save" path end here, so a later, ordinary
    /// save can never close a tab nobody asked to close.
    void abandonClose(QWidget* page);

    /// Removes `page` from the tabs and deletes it, showing the placeholder
    /// again when it was the last one.
    void removePage(QWidget* page);
    /// Puts the dirty marker in front of the tab's file name, or takes it away.
    void updateTabTitle(QWidget* page);
    /// The index of the tab showing `key`, or -1.
    int indexOfKey(const QString& key) const;

    QStackedWidget* m_stack = nullptr;
    QLabel* m_placeholder = nullptr;
    QTabWidget* m_tabs = nullptr;
    /// Never null: the constructor installs the modal box.
    std::function<Unsaved(const QString&, bool)> m_ask;
    /// Tabs whose close is waiting on a write. A second close request for one
    /// of them is ignored rather than arming a second pair of connections.
    QHash<QWidget*, PendingClose> m_closing;
};
