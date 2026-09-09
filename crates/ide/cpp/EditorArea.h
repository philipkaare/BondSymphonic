#pragma once
#include <QString>
#include <QWidget>

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

    /// Shows `path`'s diff against the base branch. Milestone 3's diff task
    /// fills this in; until then it only says so.
    void openDiff(const QString& workspaceId, const QString& path);

    /// The editor in the current tab, or null when there is none or the tab
    /// holds something that is not an editor.
    EditorWidget* currentEditor() const;

    /// Saves every dirty editor. Clean ones are left alone, so a save-all does
    /// not rewrite files nothing has touched.
    void saveAll();

    /// Closes every tab belonging to `workspaceId`, discarding unsaved edits.
    /// Used when the workspace itself is gone and there is nothing left to save
    /// to.
    void closeWorkspace(const QString& workspaceId);

    /// Closes the tab at `index`, asking first when it has unsaved edits.
    /// Returns whether the tab is gone: a cancelled prompt and a save still in
    /// flight both answer false, and the save closes the tab when the daemon
    /// confirms the write.
    bool closeTab(int index);

signals:
    /// The current tab changed. `editor` is null when no editor is showing.
    void currentEditorChanged(EditorWidget* editor);

private:
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
};
